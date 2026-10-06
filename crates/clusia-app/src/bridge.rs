//! The window's only link to the world (spec §3.1). One daemon connection runs on a background
//! thread with its own current-thread tokio runtime:
//! - `Tell`s go to Bevy over a crossbeam channel, and every one wakes the reactive event loop;
//! - `Ask`s come back over a tokio channel.
//!
//! `OpenReview`, `OpenCached`, `Publish` and `FetchMedia` run on a second, short-lived connection
//! (a task on the same runtime), so the main one keeps streaming `LoadStep` events while they
//! wait.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use clusia_core::{
    Anchor, DraftKind, Paths, PrConversation, PrFilter, PrRef, Review, Side, ThreadRef, Verdict,
};
use clusia_protocol::{
    AnchorInput, Client, ClientError, Command, ErrorCode, Event, GifPage, LoadStep, LoadStepKind,
    MediaFile, NewsItem, PublishResult, Reply, ReviewView, Secret, StepStatus, WindowTarget,
    topics,
};
use crossbeam_channel::{Receiver, Sender};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Mode;
use crate::clock::Clock;
use crate::fixture;
use crate::review_state::{self, ReviewEvent, ReviewTabs};
use crate::snapshot::{self, GiphyKey, Refresh, Snapshot};
use crate::ui::media::MediaCache;

/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(100);
pub const TOAST_SECS: f64 = 4.0;

/// The `Model.rejected` key under which a refusal Giphy gave for the stored key is kept.
pub const GIPHY_KEY_REFUSAL: &str = "giphy.key";

#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    SetConfig {
        key: String,
        value: String,
    },
    SetToken(Secret),
    ClearToken,
    SyncNow,
    RefreshAuth,
    /// The key goes straight to the Keychain; `GiphyKeyChanged` refreshes the snapshot.
    SetGiphyKey(Secret),
    ClearGiphyKey,
    OpenInEditor {
        path: String,
        line: Option<u32>,
    },
    /// After `Tell::Lost`: connect again.
    Reconnect,
    /// Worker connection: the cache first (`CachedAvailable`), then `OpenReview` and
    /// `GetWhatsNew` (`Opened` or `OpenFailed`).
    OpenReview(PrRef),
    /// Worker connection: the cached copy (`OpenedFromCache`).
    OpenCached(PrRef),
    LoadConversation(PrRef),
    MarkSeen(PrRef),
    /// Answered by `Saved` or `Refused` with the same ticket.
    AddItem {
        pr: PrRef,
        kind: DraftKind,
        anchor: Option<AnchorInput>,
        thread: Option<ThreadRef>,
        body: String,
        ticket: u64,
    },
    UpdateItem {
        pr: PrRef,
        id: String,
        body: String,
        ticket: u64,
    },
    RemoveItem {
        pr: PrRef,
        id: String,
    },
    /// Worker connection: `Published` or `PublishFailed`.
    Publish {
        pr: PrRef,
        verdict: Verdict,
        summary: String,
    },
    /// Leave the review (saved when the draft has items): `Left`.
    CloseReview(PrRef),
    Discard(PrRef),
    /// Worker connection: `Tell::Media` with this URL.
    FetchMedia(String),
    /// Giphy search (empty query: trending), answered by `Tell::Gifs`.
    SearchGifs {
        query: String,
        offset: u32,
    },
}

/// A `Tell::Gifs` on its way to the GIF popover.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct GifsArrived(pub Result<GifPage, (ErrorCode, String)>);

/// Why the daemon has no file for a picture, and whether asking again later can help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaError {
    /// The connection or the daemon's network failed; the picture itself was not turned down.
    pub transient: bool,
    pub message: String,
}

impl MediaError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            transient: true,
            message: message.into(),
        }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            transient: false,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tell {
    Snapshot(Box<Snapshot>),
    /// The answer to the last `Ask::SearchGifs`: a page, or the daemon's error code and message.
    Gifs(Result<GifPage, (ErrorCode, String)>),
    /// A config write was refused: the key and the daemon's message.
    Rejected {
        key: String,
        message: String,
    },
    /// A one-off outcome, shown as a toast.
    Notice {
        text: String,
        warning: bool,
    },
    /// Another launch asked this window to show something.
    Show(WindowTarget),
    /// The connection is gone; the UI offers `Ask::Reconnect`.
    Lost(String),
    /// The daemon is stopping on request (the tray's Quit): the window closes too.
    Quit,
    /// A `LoadStep` event, live while a review opens.
    Step(LoadStep),
    /// The cached copy, shown dimmed while the fresh one loads.
    CachedAvailable {
        pr: PrRef,
        view: Box<ReviewView>,
        fetched_at: i64,
    },
    Opened {
        pr: PrRef,
        view: Box<ReviewView>,
        news: Vec<NewsItem>,
    },
    OpenedFromCache {
        pr: PrRef,
        view: Box<ReviewView>,
        fetched_at: i64,
    },
    /// `cache`: the daemon has a cached copy of this review.
    OpenFailed {
        pr: PrRef,
        message: String,
        cache: bool,
    },
    /// The stored review changed (draft items, state) for a review this window has open.
    ReviewFile(Box<Review>),
    Conversation {
        pr: PrRef,
        conversation: PrConversation,
    },
    /// A draft write with this ticket was saved.
    Saved {
        pr: PrRef,
        ticket: u64,
    },
    /// A draft write with this ticket was refused; the editor keeps its text.
    Refused {
        pr: PrRef,
        ticket: u64,
        message: String,
    },
    Published {
        pr: PrRef,
        result: PublishResult,
    },
    PublishFailed {
        pr: PrRef,
        code: ErrorCode,
        message: String,
    },
    /// The review was closed or discarded on the daemon.
    Left(PrRef),
    /// The daemon's answer to `FetchMedia`: the file in its media cache, or why there is none.
    Media {
        url: String,
        file: Result<MediaFile, MediaError>,
    },
}

/// The ends Bevy keeps, and the connection thread.
pub struct Link {
    pub tell: Receiver<Tell>,
    pub ask: UnboundedSender<Ask>,
    pub thread: BridgeThread,
}

/// The connection thread. It ends once every `Ask` sender is gone and the asks already queued
/// are answered (the channel hands them out before it reports closed).
pub struct BridgeThread(std::thread::JoinHandle<()>);

impl BridgeThread {
    /// Waits up to `within` for the thread to end; `false` when it is still running.
    pub fn finish(self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while !self.0.is_finished() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.join();
        true
    }
}

/// Where `connect` leaves the bridge thread, so `app::run` can wait for it after the window
/// closed (leave choices sent on the way out must reach the daemon).
#[derive(Clone, Default)]
pub struct BridgeSlot(Arc<Mutex<Option<BridgeThread>>>);

impl BridgeSlot {
    pub fn take(&self) -> Option<BridgeThread> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn put(&self, thread: BridgeThread) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread);
    }
}

/// Starts the connection thread; `wake` runs after every `Tell`.
pub fn spawn(paths: Paths, home: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Link {
    let (tell_tx, tell_rx) = crossbeam_channel::unbounded();
    let (ask_tx, ask_rx) = unbounded_channel();
    let teller = Teller {
        tx: tell_tx,
        wake: Arc::new(wake),
    };
    let thread = std::thread::Builder::new()
        .name("clusia-bridge".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run(paths, home, teller, ask_rx)),
                Err(e) => teller.send(Tell::Lost(format!("cannot start the bridge: {e}"))),
            }
        })
        .expect("spawn the bridge thread");
    Link {
        tell: tell_rx,
        ask: ask_tx,
        thread: BridgeThread(thread),
    }
}

#[derive(Clone)]
struct Teller {
    tx: Sender<Tell>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Teller {
    fn send(&self, tell: Tell) {
        if self.tx.send(tell).is_ok() {
            (self.wake)();
        }
    }
}

/// The reviews this window has open on the daemon: their `ReviewChanged` / `ReviewOutdated`
/// events refetch the review file. Kept across reconnects, and shared with the workers so a
/// failed open forgets its PR.
type OpenSet = Arc<Mutex<HashSet<PrRef>>>;

fn open_set(open: &OpenSet) -> std::sync::MutexGuard<'_, HashSet<PrRef>> {
    open.lock().unwrap_or_else(|p| p.into_inner())
}

/// Sessions until Bevy drops its `Ask` sender; after a lost session, waits for `Reconnect`.
async fn run(
    paths: Paths,
    home: Option<PathBuf>,
    teller: Teller,
    mut asks: UnboundedReceiver<Ask>,
) {
    let open = OpenSet::default();
    loop {
        match session(&paths, home.as_deref(), &teller, &open, &mut asks).await {
            Ok(()) => return,
            Err(reason) => teller.send(Tell::Lost(reason)),
        }
        loop {
            match asks.recv().await {
                None => return,
                Some(Ask::Reconnect) => break,
                Some(Ask::SetConfig { key, .. }) => teller.send(Tell::Rejected {
                    key,
                    message: "Not connected to clusiad — not saved".into(),
                }),
                Some(ask) if offline(&ask, &teller) => {}
                Some(_) => teller.send(Tell::Notice {
                    text: "Not connected to clusiad — try again after reconnecting".into(),
                    warning: true,
                }),
            }
        }
    }
}

/// Answers a review ask while disconnected so its screen never waits: editors keep their text,
/// opening fails, publishing fails. `false` for the other asks.
fn offline(ask: &Ask, teller: &Teller) -> bool {
    const NOT_CONNECTED: &str = "Not connected to clusiad — not saved";
    match ask {
        Ask::AddItem { pr, ticket, .. } | Ask::UpdateItem { pr, ticket, .. } => {
            teller.send(Tell::Refused {
                pr: pr.clone(),
                ticket: *ticket,
                message: NOT_CONNECTED.into(),
            });
        }
        Ask::OpenReview(pr) => teller.send(Tell::OpenFailed {
            pr: pr.clone(),
            message: "Not connected to clusiad".into(),
            cache: false,
        }),
        Ask::Publish { pr, .. } => teller.send(Tell::PublishFailed {
            pr: pr.clone(),
            code: ErrorCode::Offline,
            message: "Not connected to clusiad — nothing was published".into(),
        }),
        Ask::FetchMedia(url) => teller.send(Tell::Media {
            url: url.clone(),
            file: Err(MediaError::transient("Not connected to clusiad")),
        }),
        Ask::SearchGifs { .. } => teller.send(Tell::Gifs(Err((
            ErrorCode::Offline,
            "Not connected to clusiad".into(),
        )))),
        _ => return false,
    }
    true
}

/// The answer to a GIF search; any reply but `Gifs` is a daemon fault, never silence.
fn gifs_tell(reply: Reply) -> Tell {
    match reply {
        Reply::Gifs(page) => Tell::Gifs(Ok(page)),
        _ => Tell::Gifs(Err((
            ErrorCode::Internal,
            "Unexpected reply from clusiad".into(),
        ))),
    }
}

/// `Ok(())` when the UI is gone; `Err(reason)` when the daemon is.
async fn session(
    paths: &Paths,
    home: Option<&Path>,
    teller: &Teller,
    open: &OpenSet,
    asks: &mut UnboundedReceiver<Ask>,
) -> Result<(), String> {
    let (mut client, _) = clusia_protocol::launcher::ensure_daemon(paths, home, crate::CLIENT_NAME)
        .await
        .map_err(|e| format!("cannot reach clusiad: {e}"))?;
    let wanted = [
        topics::CONFIG,
        topics::PRS,
        topics::SYNC,
        topics::REVIEWS,
        topics::WINDOW,
    ];
    client
        .request(Command::Subscribe {
            topics: wanted.map(String::from).to_vec(),
        })
        .await
        .map_err(lost)?;
    let mut snap = Snapshot {
        daemon_version: client.daemon_version().to_string(),
        ..Snapshot::default()
    };
    fetch(&mut client, &mut snap, Refresh::STARTUP).await?;
    if let Some(Reply::Sync(s)) = request(&mut client, Command::GetSyncStatus).await? {
        snap.sync = Some(s);
    }
    teller.send(Tell::Snapshot(Box::new(snap.clone())));
    // The daemon may have restarted while this window was away: its open reviews read again.
    let reopened: Vec<PrRef> = open_set(open).iter().cloned().collect();
    for pr in reopened {
        refetch(&mut client, &pr, teller).await?;
    }
    // The Keychain read can wait: it comes after the first snapshot.
    let lists = Refresh {
        lists: true,
        giphy: true,
        ..Refresh::default()
    };
    fetch(&mut client, &mut snap, lists).await?;
    teller.send(Tell::Snapshot(Box::new(snap.clone())));
    let mut last = snap.clone();
    loop {
        let mut todo = tokio::select! {
            event = client.next_event() => {
                let (_, event) = event.map_err(lost)?;
                follow(&mut client, open, &event, teller).await?;
                take(&mut snap, event, teller)
            }
            ask = asks.recv() => {
                let Some(ask) = ask else { return Ok(()) };
                answer(&mut client, &mut snap, teller, paths, open, ask).await?;
                Refresh::default()
            }
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        while todo.any() {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => {
                    follow(&mut client, open, &event, teller).await?;
                    todo.merge(take(&mut snap, event, teller));
                }
                Ok(Err(e)) => return Err(lost(e)),
                Err(_) => break,
            }
        }
        fetch(&mut client, &mut snap, todo).await?;
        if snap != last {
            teller.send(Tell::Snapshot(Box::new(snap.clone())));
            last = snap.clone();
        }
    }
}

/// Review events for the open tabs: load steps go to Bevy as they come; a changed or outdated
/// review that is open here is fetched again.
async fn follow(
    client: &mut Client,
    open: &OpenSet,
    event: &Event,
    teller: &Teller,
) -> Result<(), String> {
    match event {
        Event::LoadStep(step) => teller.send(Tell::Step(step.clone())),
        Event::ReviewChanged { pr, .. } | Event::ReviewOutdated { pr, .. }
            if open_set(open).contains(pr) =>
        {
            refetch(client, pr, teller).await?;
        }
        _ => {}
    }
    Ok(())
}

/// `GetReview` → `Tell::ReviewFile` (a refusal is only logged).
async fn refetch(client: &mut Client, pr: &PrRef, teller: &Teller) -> Result<(), String> {
    if let Some(Reply::ReviewFile(review)) =
        request(client, Command::GetReview { pr: pr.clone() }).await?
    {
        teller.send(Tell::ReviewFile(review));
    }
    Ok(())
}

fn take(snap: &mut Snapshot, event: Event, teller: &Teller) -> Refresh {
    if event == Event::Stopping {
        teller.send(Tell::Quit);
        return Refresh::default();
    }
    let (refresh, show) = snapshot::apply(snap, event);
    if let Some(target) = show {
        teller.send(Tell::Show(target));
    }
    refresh
}

fn lost(e: ClientError) -> String {
    match e {
        ClientError::Closed => "clusiad closed the connection".into(),
        e => e.to_string(),
    }
}

/// `Ok(None)` for a daemon-side error (logged; the session goes on); `Err` ends the session.
async fn request(client: &mut Client, cmd: Command) -> Result<Option<Reply>, String> {
    match client.request(cmd).await {
        Ok(reply) => Ok(Some(reply)),
        Err(ClientError::Server(e)) => {
            tracing::warn!(error = %e.message, "daemon request failed");
            Ok(None)
        }
        Err(e) => Err(lost(e)),
    }
}

async fn fetch(client: &mut Client, snap: &mut Snapshot, what: Refresh) -> Result<(), String> {
    if what.config
        && let Some(Reply::Config(c)) = request(client, Command::GetConfig).await?
    {
        snap.config = c;
    }
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
        if let (Some(Reply::Prs(a)), Some(Reply::Prs(m))) = (assigned, mine) {
            snap.assigned = a;
            snap.mine = m;
            snap.lists_loaded = true;
        }
    }
    if what.reviews
        && let Some(Reply::Reviews(r)) = request(client, Command::ListReviews).await?
    {
        snap.reviews = r;
    }
    if what.activity
        && let Some(Reply::Activity(a)) = request(client, Command::GetActivity).await?
    {
        snap.activity = Some(a);
    }
    if what.auth
        && let Some(Reply::Auth(a)) = request(client, Command::AuthStatus).await?
    {
        snap.auth = Some(a);
    }
    if what.giphy
        && let Some(Reply::GiphyKeyStatus(status)) =
            request(client, Command::GiphyKeyStatus).await?
    {
        snap.giphy_key = if status.configured {
            GiphyKey::Set
        } else {
            GiphyKey::Missing
        };
    }
    Ok(())
}

async fn answer(
    client: &mut Client,
    snap: &mut Snapshot,
    teller: &Teller,
    paths: &Paths,
    open: &OpenSet,
    ask: Ask,
) -> Result<(), String> {
    let auth = Refresh {
        auth: true,
        ..Refresh::default()
    };
    match ask {
        Ask::SetConfig { key, value } => {
            match client
                .request(Command::SetConfigValue {
                    key: key.clone(),
                    value,
                })
                .await
            {
                // The `ConfigChanged` event refreshes the snapshot.
                Ok(_) => {}
                Err(ClientError::Server(e)) => teller.send(Tell::Rejected {
                    key,
                    message: e.message,
                }),
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::SetToken(token) => {
            notify(
                client,
                teller,
                Command::SetToken { token },
                "Token saved in the Keychain",
            )
            .await?;
            fetch(client, snap, auth).await?;
        }
        Ask::ClearToken => {
            notify(client, teller, Command::ClearToken, "Stored token removed").await?;
            fetch(client, snap, auth).await?;
        }
        Ask::SyncNow => {
            if let Some(Reply::Sync(s)) = request(client, Command::SyncNow).await? {
                snap.sync = Some(s);
            }
        }
        Ask::RefreshAuth => fetch(client, snap, auth).await?,
        Ask::SetGiphyKey(key) => {
            notify(
                client,
                teller,
                Command::SetGiphyKey { key },
                "Giphy key saved in the Keychain",
            )
            .await?;
        }
        Ask::ClearGiphyKey => {
            notify(client, teller, Command::ClearGiphyKey, "Giphy key removed").await?;
        }
        Ask::OpenInEditor { path, line } => {
            notify(client, teller, Command::OpenInEditor { path, line }, "").await?;
        }
        Ask::Reconnect => {}
        Ask::OpenReview(ref pr) | Ask::OpenCached(ref pr) => {
            open_set(open).insert(pr.clone());
            spawn_worker(paths, teller, open, ask);
        }
        Ask::Publish { .. } | Ask::FetchMedia(_) => spawn_worker(paths, teller, open, ask),
        Ask::LoadConversation(pr) => match client
            .request(Command::GetConversation { pr: pr.clone() })
            .await
        {
            Ok(Reply::Conversation(conversation)) => {
                teller.send(Tell::Conversation { pr, conversation });
            }
            Ok(_) => {}
            Err(ClientError::Server(e)) => teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            }),
            Err(e) => return Err(lost(e)),
        },
        Ask::SearchGifs { query, offset } => {
            match client.request(Command::SearchGifs { query, offset }).await {
                Ok(reply) => teller.send(gifs_tell(reply)),
                Err(ClientError::Server(e)) => teller.send(Tell::Gifs(Err((e.code, e.message)))),
                Err(e) => {
                    teller.send(Tell::Gifs(Err((
                        ErrorCode::Offline,
                        "Not connected to clusiad".into(),
                    ))));
                    return Err(lost(e));
                }
            }
        }
        Ask::MarkSeen(pr) => {
            request(client, Command::MarkSeen { pr }).await?;
        }
        Ask::AddItem {
            pr,
            kind,
            anchor,
            thread,
            body,
            ticket,
        } => {
            let cmd = Command::AddDraftItem {
                pr: pr.clone(),
                kind,
                anchor,
                body,
                thread,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::UpdateItem {
            pr,
            id,
            body,
            ticket,
        } => {
            let cmd = Command::UpdateDraftItem {
                pr: pr.clone(),
                id,
                body,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::RemoveItem { pr, id } => {
            notify(client, teller, Command::RemoveDraftItem { pr, id }, "").await?;
        }
        Ask::CloseReview(pr) => {
            open_set(open).remove(&pr);
            // The tab closes whatever the daemon says; a refusal is only reported.
            notify(client, teller, Command::CloseReview { pr: pr.clone() }, "").await?;
            teller.send(Tell::Left(pr));
        }
        Ask::Discard(pr) => match client
            .request(Command::DiscardReview { pr: pr.clone() })
            .await
        {
            Ok(_) => {
                let left = Tell::Left(pr);
                forget_finished(open, &left);
                teller.send(left);
            }
            Err(ClientError::Server(e)) => teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            }),
            Err(e) => return Err(lost(e)),
        },
    }
    Ok(())
}

/// `AddDraftItem` / `UpdateDraftItem`: `Saved` or `Refused` with the editor's ticket.
async fn draft_write(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    pr: PrRef,
    ticket: u64,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(_) => teller.send(Tell::Saved { pr, ticket }),
        Err(ClientError::Server(e)) => teller.send(Tell::Refused {
            pr,
            ticket,
            message: e.message,
        }),
        Err(e) => return Err(lost(e)),
    }
    Ok(())
}

/// Runs a long request on its own connection, so this one keeps reading events.
fn spawn_worker(paths: &Paths, teller: &Teller, open: &OpenSet, ask: Ask) {
    tokio::spawn(work(paths.socket(), teller.clone(), open.clone(), ask));
}

/// Stops following a review that is no longer open on the daemon: it failed to open, was
/// published (its file is gone), or was left.
fn forget_finished(open: &OpenSet, tell: &Tell) {
    if let Tell::OpenFailed { pr, .. } | Tell::Published { pr, .. } | Tell::Left(pr) = tell {
        open_set(open).remove(pr);
    }
}

async fn work(socket: PathBuf, teller: Teller, open: OpenSet, ask: Ask) {
    match ask {
        Ask::OpenReview(pr) => {
            let tell = open_review(&socket, &teller, pr).await;
            forget_finished(&open, &tell);
            teller.send(tell);
        }
        Ask::OpenCached(pr) => open_cached(&socket, &teller, pr).await,
        Ask::FetchMedia(url) => teller.send(fetch_media(&socket, url).await),
        Ask::Publish {
            pr,
            verdict,
            summary,
        } => {
            let tell = publish(&socket, pr, verdict, summary).await;
            forget_finished(&open, &tell);
            teller.send(tell);
        }
        _ => {}
    }
}

async fn worker(socket: &Path) -> Result<Client, String> {
    Client::connect(socket, crate::CLIENT_NAME)
        .await
        .map_err(|e| format!("cannot reach clusiad: {e}"))
}

fn message_of(e: ClientError) -> String {
    match e {
        ClientError::Server(e) => e.message,
        e => lost(e),
    }
}

/// Sends `CachedAvailable` when there is a cached copy; gives back `Opened` or `OpenFailed`.
async fn open_review(socket: &Path, teller: &Teller, pr: PrRef) -> Tell {
    let mut client = match worker(socket).await {
        Ok(c) => c,
        Err(message) => {
            return Tell::OpenFailed {
                pr,
                message,
                cache: false,
            };
        }
    };
    let mut cache = false;
    if let Ok(Reply::Cached(cached)) = client
        .request(Command::GetCachedReview { pr: pr.clone() })
        .await
    {
        cache = true;
        let cached = *cached;
        teller.send(Tell::CachedAvailable {
            pr: pr.clone(),
            view: Box::new(cached.view),
            fetched_at: cached.fetched_at,
        });
    }
    match client.request(Command::OpenReview { pr: pr.clone() }).await {
        Ok(Reply::Review(view)) => {
            let news = match client
                .request(Command::GetWhatsNew { pr: pr.clone() })
                .await
            {
                Ok(Reply::WhatsNew(news)) => news,
                _ => Vec::new(),
            };
            Tell::Opened { pr, view, news }
        }
        Ok(_) => Tell::OpenFailed {
            pr,
            message: "clusiad sent an unexpected reply".into(),
            cache,
        },
        Err(e) => Tell::OpenFailed {
            pr,
            message: message_of(e),
            cache,
        },
    }
}

async fn open_cached(socket: &Path, teller: &Teller, pr: PrRef) {
    let reply = match worker(socket).await {
        Ok(mut client) => client
            .request(Command::GetCachedReview { pr: pr.clone() })
            .await
            .map_err(message_of),
        Err(message) => Err(message),
    };
    match reply {
        Ok(Reply::Cached(cached)) => {
            let cached = *cached;
            teller.send(Tell::OpenedFromCache {
                pr,
                view: Box::new(cached.view),
                fetched_at: cached.fetched_at,
            });
        }
        Ok(_) => {}
        Err(text) => teller.send(Tell::Notice {
            text,
            warning: true,
        }),
    }
}

/// `Media`: the daemon's cached file for `url`, or the reason it has none.
async fn fetch_media(socket: &Path, url: String) -> Tell {
    let file = match worker(socket).await {
        Err(message) => Err(MediaError::transient(message)),
        Ok(mut client) => match client
            .request(Command::FetchMedia { url: url.clone() })
            .await
        {
            Ok(Reply::Media(file)) => Ok(file),
            Ok(_) => Err(MediaError::transient("clusiad sent an unexpected reply")),
            Err(e) => Err(media_error(e)),
        },
    };
    Tell::Media { url, file }
}

/// Only the daemon's error code says whether asking again can help; every other failure is the
/// connection.
fn media_error(e: ClientError) -> MediaError {
    match e {
        ClientError::Server(e) => MediaError {
            transient: matches!(e.code, ErrorCode::Offline | ErrorCode::Internal),
            message: e.message,
        },
        e => MediaError::transient(lost(e)),
    }
}

/// `Published` or `PublishFailed`.
async fn publish(socket: &Path, pr: PrRef, verdict: Verdict, summary: String) -> Tell {
    let mut client = match worker(socket).await {
        Ok(c) => c,
        Err(message) => {
            return Tell::PublishFailed {
                pr,
                code: ErrorCode::Offline,
                message,
            };
        }
    };
    let cmd = Command::Publish {
        pr: pr.clone(),
        verdict,
        summary,
    };
    match client.request(cmd).await {
        Ok(Reply::Published(result)) => Tell::Published { pr, result },
        Ok(_) => Tell::PublishFailed {
            pr,
            code: ErrorCode::Internal,
            message: "clusiad sent an unexpected reply".into(),
        },
        Err(ClientError::Server(e)) => Tell::PublishFailed {
            pr,
            code: e.code,
            message: e.message,
        },
        Err(e) => Tell::PublishFailed {
            pr,
            code: ErrorCode::Offline,
            message: format!(
                "Lost clusiad while publishing ({}). The review may have been posted — check \
                 GitHub before publishing again.",
                lost(e)
            ),
        },
    }
}

/// Sends `cmd`. A refusal becomes a warning notice, and success shows `ok` (when not empty).
async fn notify(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    ok: &str,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(_) => {
            if !ok.is_empty() {
                teller.send(Tell::Notice {
                    text: ok.into(),
                    warning: false,
                });
            }
            Ok(())
        }
        Err(ClientError::Server(e)) => {
            teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            });
            Ok(())
        }
        Err(e) => Err(lost(e)),
    }
}

// ---- Bevy side ----

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    #[default]
    Connecting,
    Live,
    Lost(String),
}

/// Everything the screens read.
#[derive(Resource, Debug, Default)]
pub struct Model {
    pub snapshot: Snapshot,
    pub connection: Connection,
    /// The daemon's last refusal per config key, shown next to that field.
    pub rejected: HashMap<String, String>,
}

/// Where UI actions go: the bridge in live mode, `recorded` otherwise (demo mode, tests).
#[derive(Resource, Default)]
pub struct Asks {
    tx: Option<UnboundedSender<Ask>>,
    pub recorded: Vec<Ask>,
}

impl Asks {
    pub fn live(tx: UnboundedSender<Ask>) -> Self {
        Self {
            tx: Some(tx),
            recorded: Vec::new(),
        }
    }

    pub fn send(&mut self, ask: Ask) {
        match &self.tx {
            Some(tx) => {
                let _ = tx.send(ask);
            }
            None => self.recorded.push(ask),
        }
    }
}

/// Writes a config value and forgets that key's previous refusal.
pub fn set_config(asks: &mut Asks, model: &mut Model, key: &str, value: impl Into<String>) {
    model.rejected.remove(key);
    asks.send(Ask::SetConfig {
        key: key.to_string(),
        value: value.into(),
    });
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub warning: bool,
    /// `Time::elapsed_secs_f64` after which it disappears.
    pub until: f64,
}

#[derive(Resource, Debug, Default)]
pub struct Toasts(pub Vec<Toast>);

/// Another launch asked for this (through the daemon).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ShowRequested(pub WindowTarget);

/// Where `pump` reads tells from: the bridge thread, or `Outbox` (demo mode and tests).
#[derive(Resource)]
pub(crate) struct Inbox(Receiver<Tell>);

/// Tells produced inside the app (demo answers, tests); `pump` applies them next frame.
#[derive(Resource, Clone)]
pub(crate) struct Outbox(pub Sender<Tell>);

pub(crate) fn local_link() -> (Inbox, Outbox) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (Inbox(rx), Outbox(tx))
}

pub struct BridgePlugin {
    pub mode: Mode,
    pub paths: Paths,
    pub home: Option<PathBuf>,
    /// Receives the connection thread in live mode.
    pub thread: BridgeSlot,
}

impl Plugin for BridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Model>()
            .init_resource::<Toasts>()
            .add_message::<ShowRequested>()
            .add_message::<GifsArrived>()
            .add_systems(PreUpdate, pump);
        match self.mode {
            Mode::Live => {
                let (paths, home) = (self.paths.clone(), self.home.clone());
                let slot = self.thread.clone();
                app.add_systems(Startup, move |world: &mut World| {
                    connect(world, paths.clone(), home.clone(), &slot);
                });
            }
            Mode::Demo { dark } => {
                let (inbox, outbox) = local_link();
                app.init_resource::<Asks>()
                    .insert_resource(inbox)
                    .insert_resource(outbox)
                    .add_systems(
                        Startup,
                        move |mut model: ResMut<Model>, clock: Res<Clock>| {
                            model.snapshot = fixture::demo(clock.now());
                            if dark {
                                model.snapshot.config.appearance.theme =
                                    clusia_core::config::Theme::Dark;
                            }
                            model.connection = Connection::Live;
                        },
                    )
                    .add_systems(Update, demo_answers);
            }
        }
    }
}

fn connect(world: &mut World, paths: Paths, home: Option<PathBuf>, slot: &BridgeSlot) {
    let proxy = world
        .get_resource::<EventLoopProxyWrapper>()
        .map(|p| (**p).clone());
    let link = spawn(paths, home, move || {
        if let Some(proxy) = &proxy {
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        }
    });
    world.insert_resource(Inbox(link.tell));
    world.insert_resource(Asks::live(link.ask));
    slot.put(link.thread);
}

pub(crate) fn pump(
    inbox: Option<Res<Inbox>>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
    mut show: MessageWriter<ShowRequested>,
    mut exit: MessageWriter<AppExit>,
    mut reviews: MessageWriter<ReviewEvent>,
    mut media: ResMut<MediaCache>,
    mut gifs: MessageWriter<GifsArrived>,
    time: Res<Time>,
) {
    let Some(inbox) = inbox else { return };
    for tell in inbox.0.try_iter() {
        let mut events = Vec::new();
        let rest = review_state::apply(&mut tabs, &mut asks, tell, &mut events);
        reviews.write_batch(events);
        let Some(tell) = rest else { continue };
        match tell {
            Tell::Snapshot(s) => {
                if matches!(model.connection, Connection::Lost(_)) {
                    media.retry_failed();
                }
                model.snapshot = *s;
                model.connection = Connection::Live;
            }
            Tell::Gifs(answer) => {
                match &answer {
                    Err((ErrorCode::Unauthorized, message)) => {
                        model
                            .rejected
                            .insert(GIPHY_KEY_REFUSAL.to_string(), message.clone());
                    }
                    Ok(_) => {
                        model.rejected.remove(GIPHY_KEY_REFUSAL);
                    }
                    Err(_) => {}
                }
                gifs.write(GifsArrived(answer));
            }
            Tell::Rejected { key, message } => {
                model.rejected.insert(key, message);
            }
            Tell::Notice { text, warning } => toasts.0.push(Toast {
                text,
                warning,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
            Tell::Show(target) => {
                show.write(ShowRequested(target));
            }
            Tell::Lost(reason) => model.connection = Connection::Lost(reason),
            Tell::Quit => {
                exit.write(AppExit::Success);
            }
            Tell::Refused { message, .. } => toasts.0.push(Toast {
                text: message,
                warning: true,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
            Tell::Media { url, file } => media.arrive(url, file),
            _ => {} // the other review tells were applied above
        }
    }
}

/// Demo mode: config writes apply locally (refusals show like the daemon's); the review asks
/// are answered from `fixture::demo_review` through `Outbox`, so they go through `pump` like
/// the daemon's answers; other asks only say they were not sent.
pub(crate) fn demo_answers(
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    tabs: Res<ReviewTabs>,
    outbox: Res<Outbox>,
    clock: Res<Clock>,
    time: Res<Time>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if asks.recorded.is_empty() {
        return;
    }
    let now = clock.now();
    let mut tells = Vec::new();
    for ask in std::mem::take(&mut asks.recorded) {
        match ask {
            Ask::SetConfig { key, value } => {
                if let Err(message) =
                    snapshot::apply_config_locally(&mut model.snapshot.config, &key, &value)
                {
                    model.rejected.insert(key, message);
                }
            }
            Ask::SetGiphyKey(_) => model.snapshot.giphy_key = GiphyKey::Set,
            Ask::ClearGiphyKey => model.snapshot.giphy_key = GiphyKey::Missing,
            Ask::OpenReview(pr) => tells.extend(demo_open(pr, now)),
            Ask::OpenCached(pr) if pr == fixture::demo_pr() => {
                tells.push(Tell::OpenedFromCache {
                    pr,
                    view: Box::new(fixture::demo_review(now).0),
                    fetched_at: now - 3600,
                });
            }
            Ask::LoadConversation(pr) if pr == fixture::demo_pr() => {
                if let Some(conversation) = fixture::demo_review(now).0.conversation {
                    tells.push(Tell::Conversation { pr, conversation });
                }
            }
            Ask::MarkSeen(_) => {}
            Ask::AddItem {
                pr,
                kind,
                anchor,
                thread,
                body,
                ticket,
            } => tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                let anchor = anchor.map(|a| Anchor {
                    commit: match a.side {
                        Side::Right => review.head_sha.clone(),
                        Side::Left => review.base_sha.clone(),
                    },
                    path: a.path,
                    line: a.line,
                    start_line: a.start_line,
                    side: a.side,
                });
                review
                    .draft
                    .add(kind, anchor, thread, &body, now)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })),
            Ask::UpdateItem {
                pr,
                id,
                body,
                ticket,
            } => tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                review
                    .draft
                    .update_body(&id, &body)
                    .map_err(|e| e.to_string())
            })),
            Ask::RemoveItem { pr, id } => {
                let review = tabs.0.get(&pr).and_then(|t| t.ready()).map(|r| {
                    let mut review = r.view.review.clone();
                    let _ = review.draft.remove(&id);
                    review
                });
                if let Some(review) = review {
                    tells.push(Tell::ReviewFile(Box::new(review)));
                }
            }
            Ask::Publish { pr, verdict, .. } => tells.push(Tell::Published {
                result: PublishResult {
                    url: Some(format!(
                        "https://github.com/{}/{}/pull/{}#pullrequestreview-1",
                        pr.owner, pr.repo, pr.number
                    )),
                    closed: verdict == Verdict::ClosePr,
                    unresolved: Vec::new(),
                    close_error: None,
                },
                pr,
            }),
            Ask::CloseReview(pr) | Ask::Discard(pr) => tells.push(Tell::Left(pr)),
            Ask::SearchGifs { .. } => tells.push(Tell::Gifs(Err((
                ErrorCode::NotConfigured,
                "Demo mode has no Giphy key".into(),
            )))),
            Ask::FetchMedia(url) => tells.push(Tell::Media {
                url,
                file: Err(MediaError::refused("Pictures are not loaded in demo mode")),
            }),
            _ => toasts.0.push(Toast {
                text: "Demo mode: nothing is sent to the daemon".into(),
                warning: false,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
        }
    }
    if !tells.is_empty() {
        for tell in tells {
            let _ = outbox.0.send(tell);
        }
        redraw.write(RequestRedraw);
    }
}

/// The demo review opens at once (every step done, no harness); any other PR fails at the
/// repository step, which shows the failure screen.
fn demo_open(pr: PrRef, now: i64) -> Vec<Tell> {
    let step = |step, status, message: &str| {
        Tell::Step(LoadStep {
            pr: pr.clone(),
            step,
            status,
            message: Some(message.to_string()),
        })
    };
    if pr != fixture::demo_pr() {
        return vec![
            step(
                LoadStepKind::Repo,
                StepStatus::Failed,
                "Demo mode has no copy of this repository",
            ),
            Tell::OpenFailed {
                message: format!(
                    "Demo mode only has {} — nothing is fetched from GitHub.",
                    fixture::demo_pr()
                ),
                pr,
                cache: false,
            },
        ];
    }
    let (view, news) = fixture::demo_review(now);
    vec![
        step(LoadStepKind::Repo, StepStatus::Done, "~/Repos/clusia"),
        step(
            LoadStepKind::Branch,
            StepStatus::Done,
            view.worktree.as_deref().unwrap_or(""),
        ),
        step(LoadStepKind::Pr, StepStatus::Done, "7 files"),
        step(
            LoadStepKind::Agent,
            StepStatus::Skipped,
            "no harness set up",
        ),
        Tell::Opened {
            pr,
            view: Box::new(view),
            news,
        },
    ]
}

/// A draft write on the demo review: `ReviewFile` + `Saved`, or `Refused` with the message.
fn demo_edit(
    tabs: &ReviewTabs,
    pr: &PrRef,
    ticket: u64,
    edit: impl FnOnce(&mut Review) -> Result<(), String>,
) -> Vec<Tell> {
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return vec![Tell::Refused {
            pr: pr.clone(),
            ticket,
            message: "This review is not open".into(),
        }];
    };
    let mut review = ready.view.review.clone();
    match edit(&mut review) {
        Ok(()) => vec![
            Tell::ReviewFile(Box::new(review)),
            Tell::Saved {
                pr: pr.clone(),
                ticket,
            },
        ],
        Err(message) => vec![Tell::Refused {
            pr: pr.clone(),
            ticket,
            message,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gif_search_gets_an_answer() {
        let page = clusia_protocol::GifPage {
            items: Vec::new(),
            next_offset: None,
        };
        assert_eq!(gifs_tell(Reply::Gifs(page.clone())), Tell::Gifs(Ok(page)));
        assert_eq!(
            gifs_tell(Reply::Ack),
            Tell::Gifs(Err((
                ErrorCode::Internal,
                "Unexpected reply from clusiad".into()
            )))
        );
    }

    #[test]
    fn offline_gif_searches_fail_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let ask = Ask::SearchGifs {
            query: "turtle".into(),
            offset: 0,
        };
        assert!(offline(&ask, &teller));
        assert_eq!(
            rx.try_recv().ok(),
            Some(Tell::Gifs(Err((
                ErrorCode::Offline,
                "Not connected to clusiad".into()
            ))))
        );
    }

    #[tokio::test]
    async fn a_failed_open_is_no_longer_followed() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        let open = OpenSet::default();
        open_set(&open).insert(pr.clone());
        let socket = PathBuf::from("/nonexistent/clusia-test/clusiad.sock");
        work(socket, teller, open.clone(), Ask::OpenReview(pr.clone())).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::OpenFailed { pr: failed, cache: false, .. }) if failed == pr
        ));
        assert!(open_set(&open).is_empty(), "the failed PR is forgotten");
    }

    #[tokio::test]
    async fn a_media_fetch_without_a_daemon_says_why() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let url = "https://github.com/user-attachments/assets/1.png".to_string();
        let socket = PathBuf::from("/nonexistent/clusia-test/clusiad.sock");
        work(
            socket,
            teller,
            OpenSet::default(),
            Ask::FetchMedia(url.clone()),
        )
        .await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::Media { url: asked, file: Err(e) }) if asked == url && e.transient && e.message.contains("cannot reach clusiad")
        ));
    }

    #[test]
    fn only_offline_and_internal_daemon_errors_are_transient() {
        let class = |code| {
            media_error(ClientError::Server(clusia_protocol::ProtocolError {
                code,
                message: "m".into(),
            }))
            .transient
        };
        assert!(class(ErrorCode::Offline));
        assert!(class(ErrorCode::Internal));
        assert!(!class(ErrorCode::Refused));
        assert!(!class(ErrorCode::BadRequest));
        assert!(!class(ErrorCode::NotFound));
        assert!(!class(ErrorCode::NotConfigured));
        assert!(media_error(ClientError::Closed).transient);
    }

    #[test]
    fn offline_media_asks_fail_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        assert!(offline(
            &Ask::FetchMedia("https://github.com/a.png".into()),
            &teller
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::Media { file: Err(e), .. }) if e.transient && e.message == "Not connected to clusiad"
        ));
    }

    #[test]
    fn published_and_left_reviews_are_no_longer_followed() {
        let pr = fixture::demo_pr();
        let open = OpenSet::default();
        open_set(&open).insert(pr.clone());
        forget_finished(
            &open,
            &Tell::PublishFailed {
                pr: pr.clone(),
                code: ErrorCode::Upstream,
                message: "boom".into(),
            },
        );
        assert!(open_set(&open).contains(&pr), "a failed publish stays open");
        forget_finished(
            &open,
            &Tell::Published {
                pr: pr.clone(),
                result: PublishResult {
                    url: None,
                    closed: false,
                    unresolved: vec![],
                    close_error: None,
                },
            },
        );
        assert!(open_set(&open).is_empty(), "the published review is gone");
        open_set(&open).insert(pr.clone());
        forget_finished(&open, &Tell::Left(pr));
        assert!(open_set(&open).is_empty());
    }
}
