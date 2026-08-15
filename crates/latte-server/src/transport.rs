//! Unix socket transport for the latte-code server.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// A framed connection over a unix socket.
pub struct Connection {
    stream: UnixStream,
    read_buf: Vec<u8>,
}

impl Connection {
    /// Connect to a unix socket.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .context("failed to connect to server socket")?;
        Ok(Self {
            stream,
            read_buf: Vec::with_capacity(65536),
        })
    }

    /// Send a frame.
    pub async fn send(&mut self, frame: &[u8]) -> Result<()> {
        let len = frame.len() as u32;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(frame).await?;
        Ok(())
    }

    /// Receive a frame.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 64 * 1024 * 1024 {
            anyhow::bail!("frame too large: {} bytes", len);
        }
        let mut frame = vec![0u8; len];
        self.stream.read_exact(&mut frame).await?;
        Ok(Some(frame))
    }
}

/// A unix socket listener.
pub struct Listener {
    listener: UnixListener,
}

impl Listener {
    /// Bind to a unix socket.
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // Remove stale socket
        if path.exists() {
            tokio::fs::remove_file(path).await?;
        }
        let listener = UnixListener::bind(path).context("failed to bind server socket")?;
        Ok(Self { listener })
    }

    /// Accept a connection.
    pub async fn accept(&self) -> Result<Connection> {
        let (stream, _) = self.listener.accept().await?;
        Ok(Connection {
            stream,
            read_buf: Vec::with_capacity(65536),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Socket file is cleaned up by the OS when the process exits,
        // but we try to remove it for cleanliness.
    }
}
