use mqtt5_protocol::error::{MqttError, Result};

/// A combined reader/writer over a WASI TCP connection.
///
/// Uses `Pollable::ready()` to check for data availability before reading,
/// ensuring reads never block the executor. This enables cooperative
/// concurrency between multiple client handlers.
#[cfg(target_os = "wasi")]
pub struct WasiStream {
    // Drop order matters: Rust drops fields top-to-bottom.
    // WASI requires child resources (streams) to be dropped before
    // their parent (socket), so streams must come first.
    input: wasi::io::streams::InputStream,
    output: wasi::io::streams::OutputStream,
    _socket: wasi::sockets::tcp::TcpSocket,
}

#[cfg(target_os = "wasi")]
impl WasiStream {
    pub fn new(
        socket: wasi::sockets::tcp::TcpSocket,
        input: wasi::io::streams::InputStream,
        output: wasi::io::streams::OutputStream,
    ) -> Self {
        Self {
            input,
            output,
            _socket: socket,
        }
    }

    /// Non-blocking read. Uses `Pollable::ready()` to check if data is
    /// available before calling `read()`, so this never blocks the thread.
    /// Yields to the executor when no data is ready.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            // Check readiness without blocking
            let pollable = self.input.subscribe();
            if !pollable.ready() {
                // No data available -- yield so other tasks can run
                crate::executor::yield_now().await;
                continue;
            }

            // Data is available, read it
            match self.input.read(buf.len() as u64) {
                Ok(bytes) if bytes.is_empty() => {
                    // Spurious ready -- yield and retry
                    crate::executor::yield_now().await;
                }
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    return Ok(n);
                }
                Err(wasi::io::streams::StreamError::Closed) => {
                    return Ok(0); // EOF
                }
                Err(e) => {
                    return Err(MqttError::Io(format!("WASI read error: {e:?}")));
                }
            }
        }
    }

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

    /// Write bytes to the TCP stream.
    ///
    /// Uses non-blocking `check-write`/`write`/`flush` to avoid blocking
    /// the component. Spins briefly on backpressure since writes are
    /// typically fast for small MQTT packets.
    pub fn write(&self, buf: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let writable = self
                .output
                .check_write()
                .map_err(|e| MqttError::Io(format!("WASI check-write error: {e:?}")))?;

            if writable == 0 {
                // Output buffer full -- brief spin
                std::hint::spin_loop();
                continue;
            }

            let chunk_len = (buf.len() - offset).min(writable as usize);
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

// ---------------------------------------------------------------------------
// Native fallback for development/testing
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "wasi"))]
pub struct WasiStream {
    stream: std::cell::RefCell<std::net::TcpStream>,
}

#[cfg(not(target_os = "wasi"))]
impl WasiStream {
    pub fn new_from_std(stream: std::net::TcpStream) -> Self {
        stream
            .set_nonblocking(true)
            .expect("Failed to set non-blocking mode");
        Self {
            stream: std::cell::RefCell::new(stream),
        }
    }

    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        use std::io::Read;
        loop {
            match self.stream.borrow_mut().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    crate::executor::yield_now().await;
                }
                Err(e) => return Err(MqttError::Io(e.to_string())),
            }
        }
    }

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

    pub fn write(&self, buf: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut stream = self.stream.borrow_mut();
        stream
            .write_all(buf)
            .map_err(|e| MqttError::Io(e.to_string()))?;
        stream
            .flush()
            .map_err(|e| MqttError::Io(e.to_string()))?;
        Ok(())
    }
}
