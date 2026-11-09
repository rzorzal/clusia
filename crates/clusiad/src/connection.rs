//! One client session: handshake, then requests and subscribed events until either side stops.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use clusia_core::PrRef;
use clusia_protocol::{
    ClientMessage, CodecError, Command, ErrorCode, Event, MessageReader, Outcome, PROTOCOL_VERSION,
    ProtocolError, ServerMessage, write_message,
};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::task::JoinHandle;

use crate::handlers;
use crate::state::{Busy, Shared};

pub(crate) async fn serve(stream: UnixStream, shared: Arc<Shared>) {
    shared.clients.fetch_add(1, Ordering::SeqCst);
    let result = session(stream, &shared).await;
    shared.clients.fetch_sub(1, Ordering::SeqCst);
    if let Err(e) = result {
        tracing::debug!(error = %e, "client disconnected with an error");
    }
}

fn bad_request(id: u64, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Response {
        id,
        result: Outcome::Err(ProtocolError::new(ErrorCode::BadRequest, message)),
    }
}

/// Counts its connection in one of `Shared`'s listener counters while it lives. `on_zero`
/// runs when the last listener of its counter goes.
pub(crate) struct Listener {
    shared: Arc<Shared>,
    count: fn(&Shared) -> &AtomicUsize,
    on_zero: Option<fn(&Arc<Shared>)>,
}

impl Listener {
    pub(crate) fn new(
        shared: &Arc<Shared>,
        count: fn(&Shared) -> &AtomicUsize,
        on_zero: Option<fn(&Arc<Shared>)>,
    ) -> Self {
        count(shared).fetch_add(1, Ordering::SeqCst);
        Self {
            shared: shared.clone(),
            count,
            on_zero,
        }
    }

    /// A window's listener: when the last window goes, the app's systems are on notice.
    pub(crate) fn window(shared: &Arc<Shared>) -> Self {
        Self::new(
            shared,
            |s| &s.window_listeners,
            Some(crate::tests::windows_gone),
        )
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let previous = (self.count)(&self.shared).fetch_sub(1, Ordering::SeqCst);
        if previous == 1
            && let Some(on_zero) = self.on_zero
        {
            on_zero(&self.shared);
        }
    }
}

/// Lets go of every review a connection had open when the connection ends.
struct Holder {
    shared: Arc<Shared>,
    id: u64,
}

impl Holder {
    fn new(shared: &Arc<Shared>) -> Self {
        Self {
            shared: shared.clone(),
            id: shared.holds.new_holder(),
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.shared.holds.release_all(self.id);
    }
}

/// The review a command opens (`true`) or closes (`false`) for the connection that sent it.
fn hold_change(cmd: &Command) -> Option<(PrRef, bool)> {
    match cmd {
        Command::OpenReview { pr } => Some((pr.clone(), true)),
        Command::HoldReview { pr, held } => Some((pr.clone(), *held)),
        Command::CloseReview { pr }
        | Command::DiscardReview { pr }
        | Command::Publish { pr, .. } => Some((pr.clone(), false)),
        _ => None,
    }
}

/// Best-effort request id from a line that failed to parse, so the client can match the error.
fn id_of(line: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id")?.as_u64())
        .unwrap_or(0)
}

/// Writes every subscribed event already queued for this client. Used before a response and
/// before closing on shutdown, so a final event such as `Stopping` is never dropped.
async fn drain_events(
    events: &mut Receiver<(String, Event)>,
    topics: &HashSet<String>,
    w: &mut OwnedWriteHalf,
) -> Result<(), CodecError> {
    loop {
        match events.try_recv() {
            Ok((topic, event)) => {
                if topics.contains(&topic) {
                    write_message(w, &ServerMessage::Event { topic, event }).await?;
                }
            }
            Err(TryRecvError::Lagged(missed)) => {
                tracing::warn!(missed, "client too slow; events dropped");
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return Ok(()),
        }
    }
}

async fn session(stream: UnixStream, shared: &Arc<Shared>) -> Result<(), CodecError> {
    let (r, mut w) = stream.into_split();
    let mut reader = MessageReader::new(r);

    let client_name;
    match reader.next::<ClientMessage>().await {
        Ok(Some(ClientMessage::Hello {
            protocol, client, ..
        })) if protocol == PROTOCOL_VERSION => {
            client_name = client;
            let welcome = ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon: crate::VERSION.into(),
            };
            write_message(&mut w, &welcome).await?;
        }
        Ok(Some(ClientMessage::Hello { protocol, .. })) => {
            let message =
                format!("client speaks protocol {protocol}; install matching Clúsia versions");
            write_message(
                &mut w,
                &ServerMessage::Incompatible {
                    daemon_protocol: PROTOCOL_VERSION,
                    message,
                },
            )
            .await?;
            return Ok(());
        }
        Ok(Some(ClientMessage::Request { id, .. })) => {
            write_message(&mut w, &bad_request(id, "send hello first")).await?;
            return Ok(());
        }
        Ok(None) => return Ok(()),
        Err(CodecError::Json(e)) => {
            write_message(&mut w, &bad_request(0, format!("send hello first: {e}"))).await?;
            return Ok(());
        }
        Err(e) => return Err(e),
    }

    let holder = Holder::new(shared);
    let mut events = shared.events.subscribe();
    let mut shutdown = shared.shutdown.subscribe();
    let mut topics: HashSet<String> = HashSet::new();
    let mut window: Option<Listener> = None;
    let mut tray: Option<Listener> = None;
    // The request being handled: (id, whether it is Shutdown, the handler task, a claim on the
    // daemon staying up until the response is written). Requests stay sequential (the next line is read only once it is
    // answered), but events keep flowing.
    let mut pending: Option<(u64, bool, JoinHandle<Outcome>, Busy)> = None;
    loop {
        if pending.is_none() && *shutdown.borrow_and_update() {
            return drain_events(&mut events, &topics, &mut w).await;
        }
        tokio::select! {
            line = reader.next_line(), if pending.is_none() => {
                let Some(line) = line? else { return Ok(()) };
                let (id, cmd) = match serde_json::from_slice::<ClientMessage>(line) {
                    Ok(ClientMessage::Request { id, cmd }) => (id, cmd),
                    Ok(ClientMessage::Hello { .. }) => {
                        write_message(&mut w, &bad_request(0, "hello already received")).await?;
                        continue;
                    }
                    Err(e) => {
                        let id = id_of(line);
                        write_message(&mut w, &bad_request(id, format!("malformed request: {e}"))).await?;
                        continue;
                    }
                };
                if let Command::Subscribe { topics: wanted } = &cmd {
                    topics.extend(wanted.iter().cloned());
                    if window.is_none() && topics.contains(clusia_protocol::topics::WINDOW) {
                        window = Some(Listener::window(shared));
                    }
                    if tray.is_none() && topics.contains(clusia_protocol::topics::TRAY) {
                        tray = Some(Listener::new(shared, |s| &s.tray_listeners, None));
                    }
                }
                let stop = matches!(cmd, Command::Shutdown);
                let busy = shared.begin_work();
                // The handler holds its own claim: a client that goes away drops `pending` and
                // detaches the task, which still runs and must still be waited for.
                let working = shared.begin_work();
                let task_shared = shared.clone();
                let client = client_name.clone();
                let hold = hold_change(&cmd);
                let holder_id = holder.id;
                let task = tokio::spawn(async move {
                    let _working = working;
                    let outcome = handlers::handle(&task_shared, &client, cmd).await;
                    // What the answer reports is on disk before the client reads it.
                    task_shared.review_caches.settled().await;
                    if let (Outcome::Ok(_), Some((pr, held))) = (&outcome, hold) {
                        task_shared.holds.set(&pr, holder_id, held);
                    }
                    outcome
                });
                pending = Some((id, stop, task, busy));
            }
            joined = async { pending.as_mut().map(|(_, _, task, _)| task).expect("guarded").await }, if pending.is_some() => {
                let Some((id, stop, _, _busy)) = pending.take() else { continue };
                let result = joined.unwrap_or_else(|e| {
                    tracing::error!(error = %e, "a request handler failed");
                    Outcome::Err(ProtocolError::new(ErrorCode::Internal, "the request failed inside the daemon"))
                });
                // Events published while the request ran go out before its response.
                drain_events(&mut events, &topics, &mut w).await?;
                write_message(&mut w, &ServerMessage::Response { id, result }).await?;
                if stop {
                    shared.trigger_shutdown();
                }
            }
            received = events.recv() => match received {
                Ok((topic, event)) => {
                    if topics.contains(&topic) {
                        write_message(&mut w, &ServerMessage::Event { topic, event }).await?;
                    }
                }
                Err(RecvError::Lagged(missed)) => tracing::warn!(missed, "client too slow; events dropped"),
                Err(RecvError::Closed) => return Ok(()),
            },
            // While a request runs, shutdown waits for its response (checked at the loop top).
            changed = shutdown.changed(), if pending.is_none() => {
                if changed.is_err() {
                    return Ok(());
                }
                return drain_events(&mut events, &topics, &mut w).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> (tempfile::TempDir, Arc<Shared>) {
        let dir = tempfile::tempdir().unwrap();
        let options = crate::options::DaemonOptions {
            github_api: Some("http://127.0.0.1:9".into()),
            github_token: None,
            gh_program: "/nonexistent/gh".into(),
            secrets: Arc::new(clusia_platform::MemoryStore::default()),
            background_sync: false,
            tray_program: None,
            spawner: Arc::new(crate::spawner::RecordingSpawner::default()),
            media_extra_hosts: Vec::new(),
            media_allow_local: false,
            media_resolve: Vec::new(),
            giphy_api: Some("http://127.0.0.1:9".into()),
            harness_search_paths: Vec::new(),
            bridge_program: None,
            harness_env: Vec::new(),
            test_exec: Arc::new(clusia_testrun::testkit::FakeExec::new()),
            docker_program: Some("/nonexistent/docker".into()),
            sandbox_exec_program: "/nonexistent/sandbox-exec".into(),
            claude_program: Some("/nonexistent/claude".into()),
            codex_program: Some("/nonexistent/codex".into()),
            gh_config_dir: None,
        };
        let shared = Shared::new(
            clusia_core::Paths::new(dir.path()),
            clusia_core::Config::default(),
            options,
        );
        (dir, Arc::new(shared))
    }

    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn hook(_: &Arc<Shared>) {
        HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn the_last_window_listener_calls_its_hook_once() {
        let (_dir, shared) = shared();
        let first = Listener::new(&shared, |s| &s.window_listeners, Some(hook));
        let second = Listener::new(&shared, |s| &s.window_listeners, Some(hook));
        assert_eq!(shared.window_listeners.load(Ordering::SeqCst), 2);
        drop(first);
        assert_eq!(
            HOOK_CALLS.load(Ordering::SeqCst),
            0,
            "a listener is still there"
        );
        drop(second);
        assert_eq!(HOOK_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(shared.window_listeners.load(Ordering::SeqCst), 0);
        // Without a hook, the last one going calls nothing.
        drop(Listener::new(&shared, |s| &s.tray_listeners, None));
        assert_eq!(HOOK_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn opening_and_closing_a_review_changes_what_a_connection_holds() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(
            hold_change(&Command::OpenReview { pr: pr.clone() }),
            Some((pr.clone(), true))
        );
        assert_eq!(
            hold_change(&Command::CloseReview { pr: pr.clone() }),
            Some((pr.clone(), false))
        );
        assert_eq!(
            hold_change(&Command::DiscardReview { pr: pr.clone() }),
            Some((pr.clone(), false))
        );
        assert_eq!(
            hold_change(&Command::HoldReview {
                pr: pr.clone(),
                held: true
            }),
            Some((pr.clone(), true))
        );
        assert_eq!(
            hold_change(&Command::HoldReview {
                pr: pr.clone(),
                held: false
            }),
            Some((pr.clone(), false))
        );
        assert_eq!(hold_change(&Command::GetConfig), None);
    }
}
