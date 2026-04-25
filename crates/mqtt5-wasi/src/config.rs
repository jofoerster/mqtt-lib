use mqtt5::broker::config::BrokerConfig;
use std::time::Duration;

/// Configuration for the standalone WASI MQTT broker.
///
/// Wraps the subset of [`BrokerConfig`] options that are meaningful in the
/// WASI environment (no TLS, no persistent storage, no clustering).
#[derive(Debug, Clone)]
pub struct WasiBrokerConfig {
    /// Allow clients to connect without authentication.
    pub allow_anonymous: bool,
    /// Maximum number of concurrent connected clients.
    pub max_clients: usize,
}

impl Default for WasiBrokerConfig {
    fn default() -> Self {
        Self {
            allow_anonymous: true,
            max_clients: 1000,
        }
    }
}

impl WasiBrokerConfig {
    pub(crate) fn to_broker_config(&self) -> BrokerConfig {
        BrokerConfig {
            max_clients: self.max_clients,
            ..BrokerConfig::default()
        }
    }
}

/// Configuration for the standalone WASI MQTT client.
///
/// Mirrors the subset of [`mqtt5_protocol::types::ConnectOptions`] that the
/// minimal WASI client supports.
#[derive(Debug, Clone)]
pub struct WasiClientConfig {
    /// Client identifier sent in CONNECT. Empty string requests a server-assigned id (v5 only).
    pub client_id: String,
    /// MQTT protocol version: `4` for v3.1.1, `5` for v5.0.
    pub protocol_version: u8,
    /// Keep-alive interval. `Duration::ZERO` disables keep-alive.
    pub keep_alive: Duration,
    /// Clean-start flag (v5) / clean-session flag (v3.1.1).
    pub clean_start: bool,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<Vec<u8>>,
}

impl Default for WasiClientConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            protocol_version: 5,
            keep_alive: Duration::from_secs(60),
            clean_start: true,
            username: None,
            password: None,
        }
    }
}

impl WasiClientConfig {
    #[must_use]
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_protocol_version(mut self, version: u8) -> Self {
        self.protocol_version = version;
        self
    }

    #[must_use]
    pub fn with_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    #[must_use]
    pub fn with_clean_start(mut self, clean_start: bool) -> Self {
        self.clean_start = clean_start;
        self
    }

    #[must_use]
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl AsRef<[u8]>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.as_ref().to_vec());
        self
    }
}
