use std::cell::RefCell;

use mqtt5_protocol::error::{MqttError, Result};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::net::TcpStream;

/// Combined reader/writer over a `wstd::net::TcpStream`.
///
/// `wstd` implements `AsyncRead` / `AsyncWrite` for both `TcpStream` and
/// `&TcpStream`, so this wrapper exposes `read`, `read_exact`, and `write`
/// through `&self`, matching the previous custom-WASI transport API and
/// allowing the stream to live in an `Rc<WasiStream>` shared between the
/// packet read loop and other tasks.
///
/// A small `pushback` buffer lets callers return bytes they over-read so the
/// next read sees them again. This is needed by the packet decoder, which
/// reads up to 5 bytes for the fixed header and may pull in the start of the
/// next packet when the current packet has no body (PINGREQ, PINGRESP,
/// DISCONNECT-without-body, AUTH-without-properties).
pub struct WasiStream {
    inner: TcpStream,
    pushback: RefCell<Vec<u8>>,
}

impl WasiStream {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            inner: stream,
            pushback: RefCell::new(Vec::new()),
        }
    }

    /// Read up to `buf.len()` bytes. Drains the pushback buffer first; only
    /// hits the underlying WASI input stream when the pushback is empty.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        {
            let mut pb = self.pushback.borrow_mut();
            if !pb.is_empty() {
                let n = pb.len().min(buf.len());
                buf[..n].copy_from_slice(&pb[..n]);
                pb.drain(..n);
                return Ok(n);
            }
        }
        let mut reader = &self.inner;
        reader
            .read(buf)
            .await
            .map_err(|e| MqttError::Io(format!("WASI read error: {e}")))
    }

    /// Read exactly `buf.len()` bytes.
    pub async fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let n = self.read(&mut buf[total..]).await?;
            if n == 0 {
                return Err(MqttError::ConnectionClosedByPeer);
            }
            total += n;
        }
        Ok(())
    }

    /// Push bytes back so the next `read` / `read_exact` returns them before
    /// touching the socket. Used by the packet decoder when it over-reads
    /// past a packet boundary.
    pub fn pushback(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut pb = self.pushback.borrow_mut();
        // Bytes to push back come before any already-pushed-back bytes.
        let mut combined = Vec::with_capacity(bytes.len() + pb.len());
        combined.extend_from_slice(bytes);
        combined.extend_from_slice(&pb);
        *pb = combined;
    }

    /// Write all bytes and flush, yielding while the underlying WASI output
    /// stream is full.
    pub async fn write(&self, buf: &[u8]) -> Result<()> {
        let mut writer = &self.inner;
        let mut offset = 0;
        while offset < buf.len() {
            let n = writer
                .write(&buf[offset..])
                .await
                .map_err(|e| MqttError::Io(format!("WASI write error: {e}")))?;
            if n == 0 {
                return Err(MqttError::Io("WASI write returned 0".to_string()));
            }
            offset += n;
        }
        writer
            .flush()
            .await
            .map_err(|e| MqttError::Io(format!("WASI flush error: {e}")))
    }
}
