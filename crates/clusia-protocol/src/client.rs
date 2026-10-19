//! The client side of the protocol, shared by the CLI, tray and window.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::codec::{CodecError, MessageReader, write_message};
use crate::{
    ClientMessage, Command, Event, Outcome, PROTOCOL_VERSION, ProtocolError, Reply, ServerMessage,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("Clúsia daemon not running (no live socket at {0})")]
    NotRunning(PathBuf),
    #[error(
        "the daemon speaks protocol {daemon_protocol} but this client speaks {PROTOCOL_VERSION}: {message}"
    )]
    Incompatible {
        daemon_protocol: u32,
        message: String,
    },
    #[error("{}", .0.message)]
    Server(ProtocolError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("the daemon closed the connection")]
    Closed,
    #[error("unexpected message from the daemon: {0}")]
    Unexpected(String),
}

/// What one line from the daemon turned out to be.
#[allow(clippy::large_enum_variant)] // short-lived, matched at once; boxing would only add noise
enum Incoming {
    Message(ServerMessage),
    /// A response whose reply or error this client cannot decode; only the request id is known.
    UnreadableResponse(u64),
}

pub struct Client {
    reader: MessageReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    events: VecDeque<(String, Event)>,
    daemon_version: String,
}

impl Client {
    pub async fn connect(socket: &Path, client_name: &str) -> Result<Self, ClientError> {
        let stream = match UnixStream::connect(socket).await {
            Ok(s) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(ClientError::NotRunning(socket.to_path_buf()));
            }
            Err(e) => return Err(CodecError::Io(e).into()),
        };
        let (r, mut writer) = stream.into_split();
        let hello = ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client: client_name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_message(&mut writer, &hello).await?;
        let mut reader = MessageReader::new(r);
        match reader.next::<ServerMessage>().await? {
            Some(ServerMessage::Welcome { daemon, .. }) => Ok(Self {
                reader,
                writer,
                next_id: 1,
                events: VecDeque::new(),
                daemon_version: daemon,
            }),
            Some(ServerMessage::Incompatible {
                daemon_protocol,
                message,
            }) => Err(ClientError::Incompatible {
                daemon_protocol,
                message,
            }),
            Some(other) => Err(ClientError::Unexpected(format!("{other:?}"))),
            None => Err(ClientError::Closed),
        }
    }

    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    pub async fn request(&mut self, cmd: Command) -> Result<Reply, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.writer, &ClientMessage::Request { id, cmd }).await?;
        loop {
            match next_incoming(&mut self.reader).await? {
                Some(Incoming::Message(ServerMessage::Response { id: got, result }))
                    if got == id =>
                {
                    return match result {
                        Outcome::Ok(reply) => Ok(reply),
                        Outcome::Err(e) => Err(ClientError::Server(e)),
                    };
                }
                Some(Incoming::UnreadableResponse(got)) if got == id => {
                    return Err(ClientError::Unexpected(
                        "the daemon answered with a reply this client does not know".into(),
                    ));
                }
                Some(Incoming::UnreadableResponse(_)) => {}
                Some(Incoming::Message(ServerMessage::Event { topic, event })) => {
                    self.events.push_back((topic, event))
                }
                Some(Incoming::Message(other)) => {
                    return Err(ClientError::Unexpected(format!("{other:?}")));
                }
                None => return Err(ClientError::Closed),
            }
        }
    }

    /// Events that arrived while waiting for earlier responses (never blocks).
    pub fn take_events(&mut self) -> Vec<(String, Event)> {
        self.events.drain(..).collect()
    }

    pub async fn next_event(&mut self) -> Result<(String, Event), ClientError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(e);
        }
        loop {
            match next_incoming(&mut self.reader).await? {
                Some(Incoming::Message(ServerMessage::Event { topic, event })) => {
                    return Ok((topic, event));
                }
                Some(Incoming::UnreadableResponse(_)) => {}
                Some(Incoming::Message(other)) => {
                    return Err(ClientError::Unexpected(format!("{other:?}")));
                }
                None => return Err(ClientError::Closed),
            }
        }
    }
}

/// The next message from the daemon. A newer daemon may send an event or a reply this client
/// has no variant for: an unknown event is skipped (logged at debug) so the connection and its
/// event stream survive; an unreadable response is reported by id so only its request fails.
/// Anything else that does not decode is still a broken stream.
async fn next_incoming(
    reader: &mut MessageReader<OwnedReadHalf>,
) -> Result<Option<Incoming>, ClientError> {
    loop {
        let Some(line) = reader.next_line().await? else {
            return Ok(None);
        };
        let first = match serde_json::from_slice::<ServerMessage>(line) {
            Ok(message) => return Ok(Some(Incoming::Message(message))),
            Err(e) => e,
        };
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(line) else {
            return Err(CodecError::Json(first).into());
        };
        match raw.get("type").and_then(|t| t.as_str()) {
            Some("event") => {
                tracing::debug!(error = %first, "skipping a daemon event this client does not know");
            }
            Some("response") => {
                let Some(id) = raw.get("id").and_then(|i| i.as_u64()) else {
                    return Err(CodecError::Json(first).into());
                };
                tracing::debug!(id, error = %first, "a daemon response could not be read");
                return Ok(Some(Incoming::UnreadableResponse(id)));
            }
            _ => return Err(CodecError::Json(first).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, topics};
    use tokio::net::UnixListener;

    /// A one-connection fake daemon: answers hello with `greeting`, then runs `script` on each request.
    async fn fake_daemon(
        greeting: ServerMessage,
        script: fn(u64, Command) -> Vec<ServerMessage>,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = MessageReader::new(r);
            let _hello: ClientMessage = r.next().await.unwrap().unwrap();
            write_message(&mut w, &greeting).await.unwrap();
            while let Ok(Some(ClientMessage::Request { id, cmd })) = r.next::<ClientMessage>().await
            {
                for m in script(id, cmd) {
                    write_message(&mut w, &m).await.unwrap();
                }
            }
        });
        (dir, path)
    }

    fn welcome() -> ServerMessage {
        ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            daemon: "9.9.9".into(),
        }
    }

    #[tokio::test]
    async fn missing_socket_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.sock");
        assert!(
            matches!(Client::connect(&path, "t").await, Err(ClientError::NotRunning(p)) if p == path)
        );
    }

    #[tokio::test]
    async fn stale_socket_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(matches!(
            Client::connect(&path, "t").await,
            Err(ClientError::NotRunning(_))
        ));
    }

    #[tokio::test]
    async fn handshake_and_request() {
        let (_d, path) = fake_daemon(welcome(), |id, _| {
            vec![ServerMessage::Response {
                id,
                result: Outcome::Ok(Reply::Ack),
            }]
        })
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert_eq!(c.daemon_version(), "9.9.9");
        assert_eq!(c.request(Command::Shutdown).await.unwrap(), Reply::Ack);
    }

    #[tokio::test]
    async fn incompatible_daemon_is_reported() {
        let greeting = ServerMessage::Incompatible {
            daemon_protocol: 99,
            message: "update".into(),
        };
        let (_d, path) = fake_daemon(greeting, |_, _| vec![]).await;
        assert!(matches!(
            Client::connect(&path, "t").await,
            Err(ClientError::Incompatible {
                daemon_protocol: 99,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn server_error_is_surfaced() {
        let (_d, path) = fake_daemon(welcome(), |id, _| {
            vec![ServerMessage::Response {
                id,
                result: Outcome::Err(ProtocolError::new(ErrorCode::UnknownConfigKey, "nope")),
            }]
        })
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        match c.request(Command::GetConfigValue { key: "x".into() }).await {
            Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::UnknownConfigKey),
            other => panic!("expected server error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn take_events_drains_the_buffer() {
        let (_d, path) = fake_daemon(welcome(), |id, _| {
            vec![
                ServerMessage::Event {
                    topic: topics::CONFIG.into(),
                    event: Event::ConfigChanged {
                        key: "k".into(),
                        value: "v".into(),
                    },
                },
                ServerMessage::Response {
                    id,
                    result: Outcome::Ok(Reply::Ack),
                },
            ]
        })
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        c.request(Command::Subscribe {
            topics: vec![topics::CONFIG.into()],
        })
        .await
        .unwrap();
        assert_eq!(c.take_events().len(), 1);
        assert!(c.take_events().is_empty());
    }

    #[tokio::test]
    async fn events_during_a_request_are_buffered() {
        let (_d, path) = fake_daemon(welcome(), |id, _| {
            vec![
                ServerMessage::Event {
                    topic: topics::CONFIG.into(),
                    event: Event::ConfigChanged {
                        key: "k".into(),
                        value: "v".into(),
                    },
                },
                ServerMessage::Response {
                    id,
                    result: Outcome::Ok(Reply::Ack),
                },
            ]
        })
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert_eq!(
            c.request(Command::Subscribe {
                topics: vec![topics::CONFIG.into()]
            })
            .await
            .unwrap(),
            Reply::Ack
        );
        let (topic, event) = c.next_event().await.unwrap();
        assert_eq!(topic, "config");
        assert_eq!(
            event,
            Event::ConfigChanged {
                key: "k".into(),
                value: "v".into()
            }
        );
    }

    /// A daemon that writes raw lines: the wire as a newer daemon would send it.
    async fn raw_daemon(lines: &'static [&'static str]) -> (tempfile::TempDir, PathBuf) {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = MessageReader::new(r);
            let _hello: ClientMessage = r.next().await.unwrap().unwrap();
            write_message(&mut w, &welcome()).await.unwrap();
            let Ok(Some(ClientMessage::Request { id, .. })) = r.next::<ClientMessage>().await
            else {
                return;
            };
            for line in lines {
                let line = line.replace("$ID", &id.to_string());
                w.write_all(line.as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
            }
            let _ = r.next_line().await;
        });
        (dir, path)
    }

    const UNKNOWN_EVENT: &str =
        r#"{"type":"event","topic":"config","event":{"giphy_rain":{"mm":3}}}"#;
    const UNKNOWN_UNIT_EVENT: &str = r#"{"type":"event","topic":"config","event":"confetti"}"#;
    const KNOWN_EVENT: &str = r#"{"type":"event","topic":"config","event":{"config_changed":{"key":"k","value":"v","from":"older"}}}"#;
    const ACK: &str = r#"{"type":"response","id":$ID,"result":{"ok":"ack"}}"#;

    #[tokio::test]
    async fn an_unknown_event_is_skipped_while_waiting_for_a_response() {
        let (_d, path) = raw_daemon(&[UNKNOWN_EVENT, UNKNOWN_UNIT_EVENT, KNOWN_EVENT, ACK]).await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert_eq!(c.request(Command::GetConfig).await.unwrap(), Reply::Ack);
        let events = c.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].1,
            Event::ConfigChanged {
                key: "k".into(),
                value: "v".into()
            }
        );
    }

    #[tokio::test]
    async fn an_unknown_event_does_not_end_the_event_stream() {
        let (_d, path) = raw_daemon(&[ACK, UNKNOWN_EVENT, UNKNOWN_UNIT_EVENT, KNOWN_EVENT]).await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        c.request(Command::GetConfig).await.unwrap();
        let (topic, event) = c.next_event().await.unwrap();
        assert_eq!(topic, "config");
        assert!(matches!(event, Event::ConfigChanged { .. }));
    }

    #[tokio::test]
    async fn an_unknown_reply_fails_its_request_and_keeps_the_connection() {
        let (_d, path) = raw_daemon(&[
            r#"{"type":"response","id":$ID,"result":{"ok":{"hologram":[1,2]}}}"#,
            UNKNOWN_EVENT,
            KNOWN_EVENT,
        ])
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert!(matches!(
            c.request(Command::GetConfig).await,
            Err(ClientError::Unexpected(_))
        ));
        assert!(matches!(
            c.next_event().await.unwrap().1,
            Event::ConfigChanged { .. }
        ));
    }

    #[tokio::test]
    async fn an_unknown_error_code_fails_its_request_and_keeps_the_connection() {
        let (_d, path) = raw_daemon(&[
            r#"{"type":"response","id":$ID,"result":{"err":{"code":"teleport_failed","message":"x"}}}"#,
            KNOWN_EVENT,
        ])
        .await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert!(c.request(Command::GetConfig).await.is_err());
        assert!(c.next_event().await.is_ok());
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_still_ends_the_read() {
        let (_d, path) = raw_daemon(&["not json"]).await;
        let mut c = Client::connect(&path, "t").await.unwrap();
        assert!(matches!(
            c.request(Command::GetConfig).await,
            Err(ClientError::Codec(CodecError::Json(_)))
        ));
    }
}
