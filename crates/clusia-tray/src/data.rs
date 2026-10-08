//! The tray's only link to the world: one read-only daemon connection (spec §7.3).

use std::path::PathBuf;
use std::time::Duration;

use clusia_core::{PrFilter, ReviewState};
use clusia_protocol::message::PermissionStatus;
use clusia_protocol::{Client, ClientError, Command, Event, Reply, topics};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::Snapshot;
use crate::notify::Notification;

pub const CLIENT_NAME: &str = "clusia-tray";
/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Snapshot(Box<Snapshot>),
    /// The daemon rejected a config write for this key.
    WriteFailed(String),
    /// The daemon wants a banner posted.
    Notify(Box<Notification>),
    /// The `SyncNow` asked for by the UI finished (whatever its outcome).
    Refreshed,
    /// The tray must exit. `failure` asks for a non-zero status, so the daemon's supervisor
    /// restarts the tray; a clean exit means the daemon (or the UI) is gone.
    Quit {
        reason: String,
        failure: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Refresh {
    lists: bool,
    reviews: bool,
    activity: bool,
    inbox: bool,
    /// A `lists.*` config event arrived: the UI hears about it even when nothing changed,
    /// so a write echoed back to the last snapshot's value still clears its pending state.
    preferences: bool,
}

impl Refresh {
    fn merge(&mut self, other: Refresh) {
        self.lists |= other.lists;
        self.reviews |= other.reviews;
        self.activity |= other.activity;
        self.inbox |= other.inbox;
        self.preferences |= other.preferences;
    }
}

fn refresh_for(event: Event, snap: &mut Snapshot, send: &impl Fn(Update)) -> Refresh {
    match event {
        Event::Notify {
            id,
            title,
            subtitle,
            body,
            sound,
            open,
            time_sensitive,
        } => {
            send(Update::Notify(Box::new(Notification {
                id,
                title,
                subtitle,
                body,
                sound,
                open,
                time_sensitive,
            })));
            Refresh::default()
        }
        Event::InboxChanged { .. } => Refresh {
            inbox: true,
            ..Refresh::default()
        },
        Event::PrsUpdated { .. } => Refresh {
            lists: true,
            ..Refresh::default()
        },
        Event::SyncChanged(status) => {
            snap.sync = Some(status);
            Refresh::default()
        }
        Event::ReviewChanged { state, .. } => Refresh {
            reviews: true,
            activity: state == ReviewState::Published,
            ..Refresh::default()
        },
        Event::ReviewOutdated { .. } => Refresh {
            reviews: true,
            ..Refresh::default()
        },
        Event::ConfigChanged { key, value } => {
            // Only `lists.*` keys touch the snapshot.
            snap.lists.apply(&key, &value);
            Refresh {
                preferences: key.starts_with("lists."),
                ..Refresh::default()
            }
        }
        _ => Refresh::default(),
    }
}

/// What the UI asks the data loop to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// `SetConfigValue`.
    Config(String, String),
    /// What macOS lets the tray post (`NotificationPermission`).
    Permission(PermissionStatus),
    /// `MarkInboxSeen`; no ids means all of them.
    MarkInboxSeen(Vec<String>),
    SyncNow,
    PauseSync,
    ResumeSync,
    Shutdown,
}

/// The UI's requests for the daemon.
pub type Writes = UnboundedReceiver<Outgoing>;

/// Why a session ended.
struct Stop {
    reason: String,
    /// Anything but "the daemon (or the UI) is gone": the supervisor should restart the tray.
    failure: bool,
}

impl Stop {
    fn clean(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            failure: false,
        }
    }

    fn failure(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            failure: true,
        }
    }
}

const DAEMON_GONE: &str = "clusiad closed the connection";

/// A closed connection means the daemon is gone; every other error is a failure.
fn stop_for(e: ClientError) -> Stop {
    match e {
        ClientError::Closed => Stop::clean(DAEMON_GONE),
        e => Stop::failure(e.to_string()),
    }
}

/// Runs until the daemon goes away (or the UI drops `writes`), then sends `Update::Quit`
/// (always the last update).
pub async fn run(socket: PathBuf, send: impl Fn(Update) + Send + Sync + 'static, writes: Writes) {
    run_with(socket, send, writes, None).await;
}

/// Like [`run`]. With `login_agent` (the LaunchAgent file's path), a config that asks for
/// start at login while that file is missing makes the tray ask the daemon to write it:
/// opening the app is how a login item gets installed after the app was copied in by hand.
pub async fn run_with(
    socket: PathBuf,
    send: impl Fn(Update) + Send + Sync + 'static,
    writes: Writes,
    login_agent: Option<PathBuf>,
) {
    let stop = match session(&socket, &send, writes, login_agent.as_deref()).await {
        Ok(()) => Stop::clean(DAEMON_GONE),
        Err(stop) => stop,
    };
    send(Update::Quit {
        reason: stop.reason,
        failure: stop.failure,
    });
}

async fn session(
    socket: &std::path::Path,
    send: &(impl Fn(Update) + Sync),
    mut writes: Writes,
    login_agent: Option<&std::path::Path>,
) -> Result<(), Stop> {
    let mut client = Client::connect(socket, CLIENT_NAME).await.map_err(|e| {
        let reason = format!("cannot reach clusiad: {e}");
        match e {
            // No daemon (bring-up could not start one): the tray just leaves.
            ClientError::NotRunning(_) | ClientError::Closed => Stop::clean(reason),
            _ => Stop::failure(reason),
        }
    })?;
    // Without the subscription the tray would never refresh: any error ends the session.
    client
        .request(Command::Subscribe {
            topics: vec![
                topics::PRS.into(),
                topics::SYNC.into(),
                topics::REVIEWS.into(),
                topics::CONFIG.into(),
                topics::TRAY.into(),
            ],
        })
        .await
        .map_err(stop_for)?;
    let mut snap = Snapshot::default();
    if let Reply::Config(config) = request(&mut client, Command::GetConfig).await? {
        snap.host = config.github.host;
        snap.lists = config.lists;
        if let Some(agent) = login_agent
            && needs_login_agent(config.general.start_at_login, agent.exists())
        {
            request(&mut client, Command::SetStartAtLogin { on: true }).await?;
        }
    }
    fetch(
        &mut client,
        &mut snap,
        Refresh {
            reviews: true,
            activity: true,
            inbox: true,
            ..Refresh::default()
        },
    )
    .await?;
    if let Reply::Sync(status) = request(&mut client, Command::GetSyncStatus).await? {
        snap.sync = Some(status);
    }
    send(Update::Snapshot(Box::new(snap.clone())));
    // ListPrs waits for the daemon's first sync, so the lists come in a second snapshot.
    fetch(
        &mut client,
        &mut snap,
        Refresh {
            lists: true,
            ..Refresh::default()
        },
    )
    .await?;
    send(Update::Snapshot(Box::new(snap.clone())));
    let mut last = snap.clone();
    loop {
        let mut todo = tokio::select! {
            event = client.next_event() => match event {
                Ok((_, event)) => refresh_for(event, &mut snap, send),
                Err(e) => return Err(stop_for(e)),
            },
            out = writes.recv() => {
                let Some(out) = out else {
                    return Err(Stop::clean("the tray UI is gone"));
                };
                match out {
                    Outgoing::Config(key, value) => write_config(&mut client, send, key, value).await?,
                    Outgoing::Permission(status) => {
                        request(&mut client, Command::NotificationPermission { status }).await?;
                    }
                    Outgoing::MarkInboxSeen(ids) => {
                        request(&mut client, Command::MarkInboxSeen { ids }).await?;
                    }
                    Outgoing::SyncNow => {
                        if let Reply::Sync(status) = request(&mut client, Command::SyncNow).await? {
                            snap.sync = Some(status);
                        }
                        send(Update::Snapshot(Box::new(snap.clone())));
                        send(Update::Refreshed);
                        last = snap.clone();
                    }
                    Outgoing::PauseSync | Outgoing::ResumeSync => {
                        let cmd = if out == Outgoing::PauseSync {
                            Command::PauseSync
                        } else {
                            Command::ResumeSync
                        };
                        if let Reply::Sync(status) = request(&mut client, cmd).await? {
                            snap.sync = Some(status);
                            send(Update::Snapshot(Box::new(snap.clone())));
                            last = snap.clone();
                        }
                    }
                    Outgoing::Shutdown => {
                        // The daemon replies, then closes every connection: the loop ends cleanly.
                        request(&mut client, Command::Shutdown).await?;
                    }
                }
                continue;
            }
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => todo.merge(refresh_for(event, &mut snap, send)),
                Ok(Err(e)) => return Err(stop_for(e)),
                Err(_) => break,
            }
        }
        fetch(&mut client, &mut snap, todo).await?;
        if snap != last || todo.preferences {
            send(Update::Snapshot(Box::new(snap.clone())));
            last = snap.clone();
        }
    }
}

/// The config wants the app at login but no LaunchAgent exists yet.
pub fn needs_login_agent(start_at_login: bool, agent_exists: bool) -> bool {
    start_at_login && !agent_exists
}

async fn fetch(client: &mut Client, snap: &mut Snapshot, what: Refresh) -> Result<(), Stop> {
    if what.lists {
        let assigned = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Assigned,
            },
        )
        .await?;
        let mine = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Mine,
            },
        )
        .await?;
        let both = matches!((&assigned, &mine), (Reply::Prs(_), Reply::Prs(_)));
        if let Reply::Prs(prs) = assigned {
            snap.assigned = prs;
        }
        if let Reply::Prs(prs) = mine {
            snap.mine = prs;
        }
        // Loaded only once both lists really arrived.
        snap.lists_loaded |= both;
    }
    if what.reviews
        && let Reply::Reviews(reviews) = request(client, Command::ListReviews).await?
    {
        snap.reviews = reviews;
    }
    if what.activity
        && let Reply::Activity(activity) = request(client, Command::GetActivity).await?
    {
        snap.activity = Some(activity);
    }
    if what.inbox
        && let Reply::Inbox(items) = request(client, Command::GetInbox).await?
    {
        snap.inbox = items;
    }
    Ok(())
}

/// Sends one config write; a rejected value is reported to the UI and the session goes on.
async fn write_config(
    client: &mut Client,
    send: &(impl Fn(Update) + Sync),
    key: String,
    value: String,
) -> Result<(), Stop> {
    match client
        .request(Command::SetConfigValue {
            key: key.clone(),
            value,
        })
        .await
    {
        Ok(_) => Ok(()),
        Err(ClientError::Server(e)) => {
            tracing::warn!(%key, error = %e.message, "config write rejected");
            send(Update::WriteFailed(key));
            Ok(())
        }
        Err(e) => Err(stop_for(e)),
    }
}

/// A daemon-side error (e.g. one list failing) is logged and skipped; a broken connection ends the session.
async fn request(client: &mut Client, cmd: Command) -> Result<Reply, Stop> {
    match client.request(cmd).await {
        Ok(reply) => Ok(reply),
        Err(ClientError::Server(e)) => {
            tracing::warn!(error = %e.message, "daemon request failed");
            Ok(Reply::Ack)
        }
        Err(e) => Err(stop_for(e)),
    }
}
