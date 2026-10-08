//! Command → reply. Connection handling lives in `connection`.

use std::sync::atomic::Ordering;

use clusia_core::PrFilter;
use clusia_protocol::{
    AuthInfo, Command, DaemonStatus, ErrorCode, Event, Outcome, ProtocolError, Reply, TokenSource,
    topics,
};
use clusia_provider::{ProviderError, TokenOrigin};
use clusia_store::{ConfigKeyError, get_value, save_config, set_value};

use crate::activity;
use crate::first_run;
use crate::giphy;
use crate::media;
use crate::news;
use crate::notifications;
use crate::publish;
use crate::reviews;
use crate::state::Shared;
use crate::sync;
use crate::worktrees;

pub(crate) async fn handle(shared: &Shared, client: &str, cmd: Command) -> Outcome {
    match cmd {
        Command::DaemonStatus => Outcome::Ok(Reply::Status(DaemonStatus {
            version: crate::VERSION.to_string(),
            pid: std::process::id(),
            uptime_secs: shared.started.elapsed().as_secs(),
            clients: shared.clients.load(Ordering::SeqCst),
            socket: shared.paths.socket().display().to_string(),
            notifications_permission: notifications::permission(shared),
        })),
        // Shutdown is triggered after the reply is sent; clients hear `Stopping` first.
        Command::Shutdown => {
            shared.publish(topics::SYNC, Event::Stopping);
            Outcome::Ok(Reply::Ack)
        }
        // Subscribing is tracked per connection.
        Command::Subscribe { .. } => Outcome::Ok(Reply::Ack),
        Command::GetConfig => Outcome::Ok(Reply::Config(shared.config.read().await.clone())),
        Command::GetConfigValue { key } => match get_value(&*shared.config.read().await, &key) {
            Ok(value) => Outcome::Ok(Reply::Value(value)),
            Err(e) => key_error(e),
        },
        Command::SetConfigValue { key, value } => set_config_value(shared, key, value).await,
        Command::ListPrs { filter } => {
            sync::wait_for_first_sync(shared).await;
            let prs = shared.prs.read().await;
            let list = match filter {
                PrFilter::Assigned => prs.assigned.clone(),
                PrFilter::Mine => prs.mine.clone(),
            };
            Outcome::Ok(Reply::Prs(list))
        }
        Command::SyncNow => {
            let status = sync::sync_once(shared).await;
            news::check_saved_reviews(shared).await;
            notifications::after_sync(shared).await;
            Outcome::Ok(Reply::Sync(status))
        }
        Command::GetSyncStatus => Outcome::Ok(Reply::Sync(shared.sync.read().await.clone())),
        Command::PauseSync => Outcome::Ok(Reply::Sync(sync::set_paused(shared, true).await)),
        Command::ResumeSync => Outcome::Ok(Reply::Sync(sync::set_paused(shared, false).await)),
        Command::GetPr { pr } => match sync::github_client(shared).await {
            Ok(Some(gh)) => match gh.get_pr(&pr).await {
                Ok(detail) => Outcome::Ok(Reply::Pr(detail)),
                Err(e) => provider_error(e),
            },
            Ok(None) => no_token(),
            Err(e) => provider_error(e),
        },
        Command::AuthStatus => Outcome::Ok(Reply::Auth(auth_status(shared).await)),
        Command::SetToken { token } => set_token(shared, &token).await,
        Command::ClearToken => clear_token(shared).await,
        Command::PrepareWorktree { pr } => worktrees::prepare(shared, &pr).await,
        Command::OpenReview { pr } => reviews::open(shared, client, &pr).await,
        Command::GetReview { pr } => reviews::get(shared, &pr).await,
        Command::GetCachedReview { pr } => reviews::cached(shared, &pr).await,
        Command::AddDraftItem {
            pr,
            kind,
            anchor,
            body,
            thread,
        } => reviews::add_item(shared, client, &pr, kind, anchor, thread, &body).await,
        Command::UpdateDraftItem { pr, id, body } => {
            reviews::update_item(shared, &pr, &id, &body).await
        }
        Command::RemoveDraftItem { pr, id } => reviews::remove_item(shared, &pr, &id).await,
        Command::CloseReview { pr } => reviews::close(shared, client, &pr).await,
        Command::DiscardReview { pr } => reviews::discard(shared, client, &pr).await,
        Command::ListReviews => reviews::list(shared).await,
        Command::GetDiff { pr } => reviews::diff(shared, &pr).await,
        Command::GetConversation { pr } => reviews::conversation(shared, &pr).await,
        Command::Publish {
            pr,
            verdict,
            summary,
        } => publish::publish(shared, client, &pr, verdict, &summary).await,
        Command::GetWhatsNew { pr } => news::whats_new(shared, &pr).await,
        Command::MarkSeen { pr } => news::mark_seen(shared, &pr).await,
        Command::OpenWindow { target } => {
            let listeners = shared.window_listeners.load(Ordering::SeqCst);
            if listeners > 0 {
                shared.publish(topics::WINDOW, Event::WindowRequested { target });
            }
            Outcome::Ok(Reply::Delivered(listeners))
        }
        Command::OpenInEditor { path, line } => open_in_editor(shared, &path, line).await,
        Command::GetActivity => activity::summary(shared).await,
        Command::GiphyKeyStatus => giphy::key_status(shared),
        Command::FetchMedia { url } => media::fetch(shared, &url).await,
        Command::SearchGifs { query, offset } => giphy::search(shared, &query, offset).await,
        Command::SetGiphyKey { key } => giphy::set_key(shared, key.expose()).await,
        Command::ClearGiphyKey => giphy::clear_key(shared).await,
        Command::FirstRunStatus => Outcome::Ok(Reply::FirstRun(first_run::status(shared).await)),
        Command::NotificationPermission { status } => {
            notifications::set_permission(shared, status);
            Outcome::Ok(Reply::Ack)
        }
        Command::TestNotification => {
            notifications::send_test(shared).await;
            Outcome::Ok(Reply::Ack)
        }
        Command::SetStartAtLogin { on } => crate::login::set_start_at_login(shared, on).await,
        Command::GetInbox => Outcome::Ok(Reply::Inbox(notifications::inbox(shared).await)),
        Command::MarkInboxSeen { ids } => {
            notifications::mark_seen(shared, &ids).await;
            Outcome::Ok(Reply::Ack)
        }
    }
}

fn key_error(e: ConfigKeyError) -> Outcome {
    let code = match e {
        ConfigKeyError::Unknown(_) => ErrorCode::UnknownConfigKey,
        ConfigKeyError::Invalid { .. } => ErrorCode::InvalidConfigValue,
    };
    Outcome::Err(ProtocolError::new(code, e.to_string()))
}

async fn open_in_editor(shared: &Shared, path: &str, line: Option<u32>) -> Outcome {
    let bad = |message: String| Outcome::Err(ProtocolError::new(ErrorCode::BadRequest, message));
    let file = match std::fs::canonicalize(path) {
        Ok(f) => f,
        Err(e) => return bad(format!("cannot open {path}: {e}")),
    };
    let root = std::fs::canonicalize(shared.paths.root())
        .unwrap_or_else(|_| shared.paths.root().to_path_buf());
    if !file.starts_with(&root) || !file.is_file() {
        return bad(format!(
            "only files inside {} can be opened in the editor",
            root.display()
        ));
    }
    let editor = shared.config.read().await.editor.clone();
    let argv = match clusia_core::editor_argv(&editor, &file.to_string_lossy(), line) {
        Ok(argv) => argv,
        Err(e) => {
            return Outcome::Err(ProtocolError::new(
                ErrorCode::InvalidConfigValue,
                e.to_string(),
            ));
        }
    };
    match shared.spawner.spawn(&argv) {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not start {}: {e}", argv[0]),
        )),
    }
}

pub(crate) async fn set_config_value(shared: &Shared, key: String, raw: String) -> Outcome {
    let mut config = shared.config.write().await;
    let updated = match set_value(&config, &key, &raw) {
        Ok(c) => c,
        Err(e) => return key_error(e),
    };
    if let Err(e) = save_config(&shared.paths, &updated) {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not save config.toml: {e}"),
        ));
    }
    let rendered = get_value(&updated, &key).unwrap_or(raw);
    *config = updated;
    // A new host or sign-in method may change what the token resolves to; without a token
    // the loop would otherwise sit out its idle retry.
    let resync = key.starts_with("github.");
    shared.publish(
        topics::CONFIG,
        Event::ConfigChanged {
            key,
            value: rendered.clone(),
        },
    );
    if resync {
        shared.sync_now.notify_one();
    }
    Outcome::Ok(Reply::Value(rendered))
}

pub(crate) fn provider_error(e: ProviderError) -> Outcome {
    let code = match &e {
        ProviderError::Unauthorized => ErrorCode::Unauthorized,
        ProviderError::RateLimited { .. } => ErrorCode::RateLimited,
        ProviderError::NotFound(_) => ErrorCode::NotFound,
        ProviderError::Offline(_) => ErrorCode::Offline,
        ProviderError::Http { .. }
        | ProviderError::Decode(_)
        | ProviderError::GraphQl(_)
        | ProviderError::EmptyPayload(_) => ErrorCode::Upstream,
    };
    Outcome::Err(ProtocolError::new(code, e.to_string()))
}

pub(crate) fn no_token() -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::Unauthorized, sync::NO_TOKEN))
}

fn token_source(origin: TokenOrigin) -> TokenSource {
    match origin {
        TokenOrigin::Env => TokenSource::Env,
        TokenOrigin::GhCli => TokenSource::GhCli,
        TokenOrigin::Pat => TokenSource::Pat,
    }
}

async fn auth_status(shared: &Shared) -> AuthInfo {
    let missing = |error: String| AuthInfo {
        source: None,
        login: None,
        scopes: Vec::new(),
        error: Some(error),
    };
    match sync::github_client(shared).await {
        Ok(None) => missing(sync::NO_TOKEN.to_string()),
        Err(e) => missing(e.to_string()),
        Ok(Some(gh)) => {
            let source = Some(token_source(gh.token().origin));
            match gh.viewer().await {
                Ok(v) => AuthInfo {
                    source,
                    login: Some(v.login),
                    scopes: v.scopes,
                    error: None,
                },
                Err(e) => AuthInfo {
                    source,
                    login: None,
                    scopes: Vec::new(),
                    error: Some(e.to_string()),
                },
            }
        }
    }
}

async fn set_token(shared: &Shared, token: &str) -> Outcome {
    let token = token.trim();
    if token.is_empty() {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::BadRequest,
            "the token is empty",
        ));
    }
    let host = shared.config.read().await.github.host.clone();
    if let Err(e) = shared.secrets.set(&host, token) {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not store the token: {e}"),
        ));
    }
    shared.sync_now.notify_one();
    Outcome::Ok(Reply::Ack)
}

async fn clear_token(shared: &Shared) -> Outcome {
    let host = shared.config.read().await.github.host.clone();
    match shared.secrets.delete(&host) {
        Ok(_) => {
            shared.sync_now.notify_one();
            Outcome::Ok(Reply::Ack)
        }
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not remove the token: {e}"),
        )),
    }
}
