//! One client session: handshake, then requests and subscribed events until either side stops.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clusia_protocol::{
    ClientMessage, CodecError, Command, ErrorCode, MessageReader, Outcome, PROTOCOL_VERSION,
    ProtocolError, ServerMessage, write_message,
};
use tokio::net::UnixStream;
use tokio::sync::broadcast::error::RecvError;

use crate::handlers;
use crate::state::Shared;

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

/// Best-effort request id from a line that failed to parse, so the client can match the error.
fn id_of(line: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id")?.as_u64())
        .unwrap_or(0)
}

async fn session(stream: UnixStream, shared: &Shared) -> Result<(), CodecError> {
    let (r, mut w) = stream.into_split();
    let mut reader = MessageReader::new(r);

    match reader.next::<ClientMessage>().await {
        Ok(Some(ClientMessage::Hello { protocol, .. })) if protocol == PROTOCOL_VERSION => {
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

    let mut events = shared.events.subscribe();
    let mut shutdown = shared.shutdown.subscribe();
    let mut topics: HashSet<String> = HashSet::new();
    loop {
        if *shutdown.borrow_and_update() {
            return Ok(());
        }
        tokio::select! {
            line = reader.next_line() => {
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
                }
                let stop = matches!(cmd, Command::Shutdown);
                let result = handlers::handle(shared, cmd).await;
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
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
        }
    }
}
