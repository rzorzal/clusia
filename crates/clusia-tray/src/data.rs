//! The tray's only link to the world: one read-only daemon connection (spec §7.3).

use std::path::PathBuf;
use std::time::Duration;

use clusia_core::{PrFilter, ReviewState};
use clusia_protocol::{Client, ClientError, Command, Event, Reply, topics};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::Snapshot;

pub const CLIENT_NAME: &str = "clusia-tray";
/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Snapshot(Box<Snapshot>),
    /// The daemon rejected a config write for this key.
    WriteFailed(String),
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
    /// A `lists.*` config event arrived: the UI hears about it even when nothing changed,
    /// so a write echoed back to the last snapshot's value still clears its pending state.
    preferences: bool,
}

impl Refresh {
    fn merge(&mut self, other: Refresh) {
        self.lists |= other.lists;
        self.reviews |= other.reviews;
        self.activity |= other.activity;
        self.preferences |= other.preferences;
    }
}

fn refresh_for(event: Event, snap: &mut Snapshot) -> Refresh {
    match event {
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

/// Config writes queued by the UI: `(key, value)`, sent as `SetConfigValue`.
pub type Writes = UnboundedReceiver<(String, String)>;

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
    let stop = match session(&socket, &send, writes).await {
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
) -> Result<(), Stop> {
    let mut client = Client::connect(socket, CLIENT_NAME).await.map_err(|e| {
        let reason = format!("cannot reach clusiad: {e}");
        match e {
            // No daemon: the tray never starts one, so it just leaves.
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
            ],
        })
        .await
        .map_err(stop_for)?;
    let mut snap = Snapshot::default();
    if let Reply::Config(config) = request(&mut client, Command::GetConfig).await? {
        snap.host = config.github.host;
        snap.lists = config.lists;
    }
    fetch(
        &mut client,
        &mut snap,
        Refresh {
            reviews: true,
            activity: true,
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
                Ok((_, event)) => refresh_for(event, &mut snap),
                Err(e) => return Err(stop_for(e)),
            },
            write = writes.recv() => {
                let Some((key, value)) = write else {
                    return Err(Stop::clean("the tray UI is gone"));
                };
                write_config(&mut client, send, key, value).await?;
                continue;
            }
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => todo.merge(refresh_for(event, &mut snap)),
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
