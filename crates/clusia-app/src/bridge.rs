//! The window's only link to the world (spec §3.1). One daemon connection runs on a background
//! thread with its own current-thread tokio runtime:
//! - `Tell`s go to Bevy over a crossbeam channel, and every one wakes the reactive event loop;
//! - `Ask`s come back over a tokio channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use clusia_core::{Paths, PrFilter};
use clusia_protocol::{Client, ClientError, Command, Reply, Secret, WindowTarget, topics};
use crossbeam_channel::{Receiver, Sender};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Mode;
use crate::clock::Clock;
use crate::fixture;
use crate::snapshot::{self, Refresh, Snapshot};

/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(100);
pub const TOAST_SECS: f64 = 4.0;

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
    OpenInEditor {
        path: String,
        line: Option<u32>,
    },
    /// After `Tell::Lost`: connect again.
    Reconnect,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tell {
    Snapshot(Box<Snapshot>),
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
}

/// The ends Bevy keeps.
pub struct Link {
    pub tell: Receiver<Tell>,
    pub ask: UnboundedSender<Ask>,
}

/// Starts the connection thread; `wake` runs after every `Tell`.
pub fn spawn(paths: Paths, home: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Link {
    let (tell_tx, tell_rx) = crossbeam_channel::unbounded();
    let (ask_tx, ask_rx) = unbounded_channel();
    let teller = Teller {
        tx: tell_tx,
        wake: Arc::new(wake),
    };
    std::thread::Builder::new()
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

/// Sessions until Bevy drops its `Ask` sender; after a lost session, waits for `Reconnect`.
async fn run(
    paths: Paths,
    home: Option<PathBuf>,
    teller: Teller,
    mut asks: UnboundedReceiver<Ask>,
) {
    loop {
        match session(&paths, home.as_deref(), &teller, &mut asks).await {
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
                Some(_) => teller.send(Tell::Notice {
                    text: "Not connected to clusiad — try again after reconnecting".into(),
                    warning: true,
                }),
            }
        }
    }
}

/// `Ok(())` when the UI is gone; `Err(reason)` when the daemon is.
async fn session(
    paths: &Paths,
    home: Option<&Path>,
    teller: &Teller,
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
    let lists = Refresh {
        lists: true,
        ..Refresh::default()
    };
    fetch(&mut client, &mut snap, lists).await?;
    teller.send(Tell::Snapshot(Box::new(snap.clone())));
    let mut last = snap.clone();
    loop {
        let mut todo = tokio::select! {
            event = client.next_event() => {
                let (_, event) = event.map_err(lost)?;
                take(&mut snap, event, teller)
            }
            ask = asks.recv() => {
                let Some(ask) = ask else { return Ok(()) };
                answer(&mut client, &mut snap, teller, ask).await?;
                Refresh::default()
            }
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        while todo.any() {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => todo.merge(take(&mut snap, event, teller)),
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

fn take(snap: &mut Snapshot, event: clusia_protocol::Event, teller: &Teller) -> Refresh {
    if event == clusia_protocol::Event::Stopping {
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
    Ok(())
}

async fn answer(
    client: &mut Client,
    snap: &mut Snapshot,
    teller: &Teller,
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
        Ask::OpenInEditor { path, line } => {
            notify(client, teller, Command::OpenInEditor { path, line }, "").await?;
        }
        Ask::Reconnect => {}
    }
    Ok(())
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

#[derive(Resource)]
struct Inbox(Receiver<Tell>);

pub struct BridgePlugin {
    pub mode: Mode,
    pub paths: Paths,
    pub home: Option<PathBuf>,
}

impl Plugin for BridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Model>()
            .init_resource::<Toasts>()
            .add_message::<ShowRequested>()
            .add_systems(PreUpdate, pump);
        match self.mode {
            Mode::Live => {
                let (paths, home) = (self.paths.clone(), self.home.clone());
                app.add_systems(Startup, move |world: &mut World| {
                    connect(world, paths.clone(), home.clone());
                });
            }
            Mode::Demo { dark } => {
                app.init_resource::<Asks>()
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

fn connect(world: &mut World, paths: Paths, home: Option<PathBuf>) {
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
}

fn pump(
    inbox: Option<Res<Inbox>>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    mut show: MessageWriter<ShowRequested>,
    mut exit: MessageWriter<AppExit>,
    time: Res<Time>,
) {
    let Some(inbox) = inbox else { return };
    for tell in inbox.0.try_iter() {
        match tell {
            Tell::Snapshot(s) => {
                model.snapshot = *s;
                model.connection = Connection::Live;
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
        }
    }
}

/// Demo mode: config writes apply locally (refusals show like the daemon's); other asks only
/// say they were not sent.
fn demo_answers(
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    if asks.recorded.is_empty() {
        return;
    }
    for ask in std::mem::take(&mut asks.recorded) {
        match ask {
            Ask::SetConfig { key, value } => {
                if let Err(message) =
                    snapshot::apply_config_locally(&mut model.snapshot.config, &key, &value)
                {
                    model.rejected.insert(key, message);
                }
            }
            _ => toasts.0.push(Toast {
                text: "Demo mode: nothing is sent to the daemon".into(),
                warning: false,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
        }
    }
}
