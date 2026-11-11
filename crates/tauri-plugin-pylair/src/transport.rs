//! Length-prefixed frame transport (protocol §1-2).
//!
//! A `Transport` is the byte-level channel only — the supervisor drives the
//! message semantics. Swapping stdio for a socket/pipe later means writing
//! another `Transport`, nothing else.

use async_trait::async_trait;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[async_trait]
pub trait Transport: Send {
    /// Next frame body, or None on clean EOF (peer gone).
    async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>>;
    async fn write_frame(&mut self, body: &[u8]) -> io::Result<()>;
}

/// Error kind for a length prefix above the cap — mapped to
/// PROTOCOL_VIOLATION by the supervisor.
#[derive(Debug)]
pub struct FrameTooLarge(pub usize);

impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "frame length {} exceeds cap", self.0)
    }
}
impl std::error::Error for FrameTooLarge {}

/// stdio-backed transport over a spawned child's pipes.
pub struct StdioTransport<R: AsyncReadExt + Unpin + Send, W: AsyncWriteExt + Unpin + Send> {
    reader: R,
    writer: W,
    max_frame: usize,
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncReadExt + Unpin + Send,
    W: AsyncWriteExt + Unpin + Send,
{
    pub fn new(reader: R, writer: W, max_frame: usize) -> Self {
        Self {
            reader,
            writer,
            max_frame,
        }
    }
}

#[async_trait]
impl<R, W> Transport for StdioTransport<R, W>
where
    R: AsyncReadExt + Unpin + Send,
    W: AsyncWriteExt + Unpin + Send,
{
    async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_be_bytes(len_buf) as usize;
        if n > self.max_frame {
            return Err(io::Error::new(io::ErrorKind::InvalidData, FrameTooLarge(n)));
        }
        let mut body = vec![0u8; n];
        self.reader.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    async fn write_frame(&mut self, body: &[u8]) -> io::Result<()> {
        self.writer
            .write_all(&(body.len() as u32).to_be_bytes())
            .await?;
        self.writer.write_all(body).await?;
        self.writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::Cursor;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (_rx, _tx) = duplex(1024);
        let mut raw = Vec::new();
        raw.extend_from_slice(&5u32.to_be_bytes());
        raw.extend_from_slice(b"hello");
        let cursor = Cursor::new(raw);
        let (wrx, _wtx) = duplex(1024);
        let mut t = StdioTransport::new(cursor, wrx, 1024);
        assert_eq!(t.read_frame().await.unwrap().unwrap(), b"hello");
        assert!(t.read_frame().await.unwrap().is_none()); // EOF
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let cursor = Cursor::new(64u32.to_be_bytes().to_vec());
        let (wrx, _wtx) = duplex(1024);
        let mut t = StdioTransport::new(cursor, wrx, 16);
        let err = t.read_frame().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn write_prefixes_length() {
        let (w_end, mut host_side) = duplex(64);
        let mut t = StdioTransport::new(Cursor::new(Vec::new()), w_end, 1024);
        t.write_frame(b"hi").await.unwrap();
        drop(t);
        let mut buf = [0u8; 6];
        host_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf[..4], &2u32.to_be_bytes());
        assert_eq!(&buf[4..], b"hi");
    }
}
