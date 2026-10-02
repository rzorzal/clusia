//! JSON-lines framing with a hard cap on line length.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest accepted line. Larger lines mean a broken or hostile client.
pub const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("message longer than {MAX_LINE_BYTES} bytes")]
    TooLarge,
}

pub async fn write_message<W, T>(w: &mut W, msg: &T) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

pub struct MessageReader<R> {
    inner: BufReader<R>,
    buf: Vec<u8>,
    /// `buf` holds a complete line that was already returned.
    line_ready: bool,
}

impl<R: AsyncRead + Unpin> MessageReader<R> {
    pub fn new(r: R) -> Self {
        Self {
            inner: BufReader::new(r),
            buf: Vec::new(),
            line_ready: false,
        }
    }

    /// The next line without its `\n`, or `None` at end of stream.
    ///
    /// Cancel-safe: if this future is dropped mid-line (e.g. another `tokio::select!` branch won),
    /// the bytes read so far stay in the buffer and the next call continues the same line.
    pub async fn next_line(&mut self) -> Result<Option<&[u8]>, CodecError> {
        if self.line_ready {
            self.buf.clear();
            self.line_ready = false;
        }
        let budget = (MAX_LINE_BYTES + 1).saturating_sub(self.buf.len() as u64);
        let n = (&mut self.inner)
            .take(budget)
            .read_until(b'\n', &mut self.buf)
            .await?;
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
        } else if self.buf.len() as u64 > MAX_LINE_BYTES {
            return Err(CodecError::TooLarge);
        } else if n == 0 && self.buf.is_empty() {
            return Ok(None);
        }
        self.line_ready = true;
        Ok(Some(&self.buf))
    }

    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, CodecError> {
        match self.next_line().await? {
            None => Ok(None),
            Some(line) => Ok(Some(serde_json::from_slice(line)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMessage, Command};

    #[tokio::test]
    async fn writes_and_reads_messages_in_order() {
        let (mut a, b) = tokio::io::duplex(1024);
        let m1 = ClientMessage::Request {
            id: 1,
            cmd: Command::DaemonStatus,
        };
        let m2 = ClientMessage::Request {
            id: 2,
            cmd: Command::GetConfig,
        };
        write_message(&mut a, &m1).await.unwrap();
        write_message(&mut a, &m2).await.unwrap();
        drop(a);
        let mut r = MessageReader::new(b);
        assert_eq!(r.next::<ClientMessage>().await.unwrap(), Some(m1));
        assert_eq!(r.next::<ClientMessage>().await.unwrap(), Some(m2));
        assert_eq!(r.next::<ClientMessage>().await.unwrap(), None);
    }

    #[tokio::test]
    async fn final_line_without_newline_is_returned() {
        let (mut a, b) = tokio::io::duplex(1024);
        a.write_all(br#"{"type":"request","id":3,"cmd":"shutdown"}"#)
            .await
            .unwrap();
        drop(a);
        let mut r = MessageReader::new(b);
        assert_eq!(
            r.next::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::Request {
                id: 3,
                cmd: Command::Shutdown
            })
        );
    }

    #[tokio::test]
    async fn malformed_line_errors_and_next_line_still_reads() {
        let (mut a, b) = tokio::io::duplex(1024);
        a.write_all(b"not json\n").await.unwrap();
        write_message(
            &mut a,
            &ClientMessage::Request {
                id: 4,
                cmd: Command::GetConfig,
            },
        )
        .await
        .unwrap();
        drop(a);
        let mut r = MessageReader::new(b);
        assert!(matches!(
            r.next::<ClientMessage>().await,
            Err(CodecError::Json(_))
        ));
        assert!(matches!(
            r.next::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::Request { id: 4, .. })
        ));
    }

    #[tokio::test]
    async fn cancelled_read_keeps_partial_line() {
        let (mut a, b) = tokio::io::duplex(1024);
        let mut r = MessageReader::new(b);
        a.write_all(br#"{"type":"request","id":5,"#).await.unwrap();
        tokio::select! {
            _ = r.next_line() => panic!("the line is not complete yet"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        a.write_all(br#""cmd":"get_config"}"#).await.unwrap();
        a.write_all(b"\n").await.unwrap();
        assert_eq!(
            r.next::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::Request {
                id: 5,
                cmd: Command::GetConfig
            })
        );
    }

    #[tokio::test]
    async fn oversized_line_is_rejected() {
        let (mut a, b) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'a'; 64 * 1024];
            let mut sent = 0u64;
            while sent <= MAX_LINE_BYTES {
                if a.write_all(&chunk).await.is_err() {
                    break;
                }
                sent += chunk.len() as u64;
            }
        });
        let mut r = MessageReader::new(b);
        assert!(matches!(r.next_line().await, Err(CodecError::TooLarge)));
        drop(r);
        writer.await.unwrap();
    }
}
