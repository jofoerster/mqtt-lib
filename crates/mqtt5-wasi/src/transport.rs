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
pub struct WasiStream {
    inner: TcpStream,
}

impl WasiStream {
    pub fn new(stream: TcpStream) -> Self {
        Self { inner: stream }
    }

    /// Read up to `buf.len()` bytes. Yields to the executor while waiting on
    /// the underlying WASI input stream's `Pollable`.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
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
