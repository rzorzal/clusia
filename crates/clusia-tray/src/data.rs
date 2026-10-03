//! The tray's only link to the world: one read-only daemon connection (spec §7.3).

use std::path::PathBuf;
use std::time::Duration;

use clusia_core::{PrFilter, ReviewState};
use clusia_protocol::{Client, ClientError, Command, Event, Reply, topics};

use crate::model::Snapshot;

pub const CLIENT_NAME: &str = "clusia-tray";
/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Snapshot(Snapshot),
    /// The tray must exit (the daemon is gone or unreachable).
    Quit(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Refresh {
    lists: bool,
    reviews: bool,
    activity: bool,
}

impl Refresh {
    fn merge(&mut self, other: Refresh) {
        self.lists |= other.lists;
        self.reviews |= other.reviews;
        self.activity |= other.activity;
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
        _ => Refresh::default(),
    }
}

/// Runs until the daemon goes away, then sends `Update::Quit` (always the last update).
pub async fn run(socket: PathBuf, send: impl Fn(Update) + Send + Sync + 'static) {
    let reason = match session(&socket, &send).await {
        Ok(()) => "clusiad closed the connection".to_string(),
        Err(e) => e,
    };
    send(Update::Quit(reason));
}

async fn session(socket: &std::path::Path, send: &(impl Fn(Update) + Sync)) -> Result<(), String> {
    let mut client = Client::connect(socket, CLIENT_NAME)
        .await
        .map_err(|e| format!("cannot reach clusiad: {e}"))?;
    request(
        &mut client,
        Command::Subscribe {
            topics: vec![
                topics::PRS.into(),
                topics::SYNC.into(),
                topics::REVIEWS.into(),
            ],
        },
    )
    .await?;
    let mut snap = Snapshot::default();
    if let Reply::Config(config) = request(&mut client, Command::GetConfig).await? {
        snap.host = config.github.host;
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
    send(Update::Snapshot(snap.clone()));
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
    send(Update::Snapshot(snap.clone()));
    let mut last = snap.clone();
    loop {
        let mut todo = match client.next_event().await {
            Ok((_, event)) => refresh_for(event, &mut snap),
            Err(ClientError::Closed) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => todo.merge(refresh_for(event, &mut snap)),
                Ok(Err(ClientError::Closed)) => return Ok(()),
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => break,
            }
        }
        fetch(&mut client, &mut snap, todo).await?;
        if snap != last {
            send(Update::Snapshot(snap.clone()));
            last = snap.clone();
        }
    }
}

async fn fetch(client: &mut Client, snap: &mut Snapshot, what: Refresh) -> Result<(), String> {
    if what.lists {
        if let Reply::Prs(prs) = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Assigned,
            },
        )
        .await?
        {
            snap.assigned = prs;
        }
        if let Reply::Prs(prs) = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Mine,
            },
        )
        .await?
        {
            snap.mine = prs;
        }
        snap.lists_loaded = true;
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

/// A daemon-side error (e.g. one list failing) is logged and skipped; a broken connection ends the session.
async fn request(client: &mut Client, cmd: Command) -> Result<Reply, String> {
    match client.request(cmd).await {
        Ok(reply) => Ok(reply),
        Err(ClientError::Server(e)) => {
            tracing::warn!(error = %e.message, "daemon request failed");
            Ok(Reply::Ack)
        }
        Err(ClientError::Closed) => Err("clusiad closed the connection".into()),
        Err(e) => Err(e.to_string()),
    }
}
