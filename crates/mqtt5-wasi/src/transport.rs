use mqtt5_protocol::error::{MqttError, Result};
use wasi::io::streams::{InputStream, OutputStream, StreamError};
use wasi::sockets::tcp::TcpSocket;

/// Combined reader/writer over a WASI TCP connection.
///
/// Uses `Pollable::ready()` to check for data availability before reading,
/// ensuring reads never block the executor. This enables cooperative
/// concurrency between multiple client handlers.
pub struct WasiStream {
    // Drop order matters: Rust drops fields top-to-bottom.
    // WASI requires child resources (streams) to be dropped before
    // their parent (socket), so streams must come first.
    input: InputStream,
    output: OutputStream,
    _socket: TcpSocket,
}

impl WasiStream {
    pub fn new(socket: TcpSocket, input: InputStream, output: OutputStream) -> Self {
        Self {
            input,
            output,
            _socket: socket,
        }
    }

    /// Non-blocking read using `Pollable::ready()` to check data availability
    /// before calling `read()`, so the executor is never blocked.
    /// Yields to the executor when no data is ready.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if !self.input.subscribe().ready() {
                crate::executor::yield_now().await;
                continue;
            }

            match self.input.read(buf.len() as u64) {
                Ok(bytes) if bytes.is_empty() => {
                    crate::executor::yield_now().await;
                }
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    return Ok(n);
                }
                Err(StreamError::Closed) => return Ok(0),
                Err(e) => return Err(MqttError::Io(format!("WASI read error: {e:?}"))),
            }
        }
    }

    /// Read exactly `buf.len()` bytes, yielding between partial reads.
    pub async fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut total_read = 0;
        while total_read < buf.len() {
            let n = self.read(&mut buf[total_read..]).await?;
            if n == 0 {
                return Err(MqttError::ConnectionClosedByPeer);
            }
            total_read += n;
        }
        Ok(())
    }

    /// Write bytes using non-blocking `check_write`/`write` with a blocking flush.
    pub fn write(&self, buf: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let writable = self
                .output
                .check_write()
                .map_err(|e| MqttError::Io(format!("WASI check-write error: {e:?}")))?;

            if writable == 0 {
                std::hint::spin_loop();
                continue;
            }

            let chunk_len =
                (buf.len() - offset).min(usize::try_from(writable).unwrap_or(usize::MAX));
            self.output
                .write(&buf[offset..offset + chunk_len])
                .map_err(|e| MqttError::Io(format!("WASI write error: {e:?}")))?;
            offset += chunk_len;
        }

        self.output
            .blocking_flush()
            .map_err(|e| MqttError::Io(format!("WASI flush error: {e:?}")))
    }
}
