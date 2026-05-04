use crate::client_handler::WasiClientHandler;
use crate::config::WasiBrokerConfig;
use crate::transport::WasiStream;
use mqtt5::broker::acl::AclManager;
use mqtt5::broker::auth::{
    AllowAllAuthProvider, AuthProvider, ComprehensiveAuthProvider, PasswordAuthProvider,
};
use mqtt5::broker::config::BrokerConfig;
use mqtt5::broker::resource_monitor::{ResourceLimits, ResourceMonitor};
use mqtt5::broker::router::MessageRouter;
use mqtt5::broker::storage::{DynamicStorage, MemoryBackend};
use mqtt5::broker::sys_topics::BrokerStats;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use tracing::{error, info};
use wstd::iter::AsyncIterator;
use wstd::net::TcpListener;
use wstd::runtime::{block_on, spawn};

/// Standalone WASI MQTT v5.0 broker.
///
/// Built on top of the `wstd` async runtime: `TcpListener` accepts connections
/// on the WASI 0.2 sockets API and each client is driven on a `wstd::runtime`
/// task.
pub struct WasiBroker {
    config: Arc<RwLock<BrokerConfig>>,
    router: Arc<MessageRouter>,
    auth_provider: Arc<dyn AuthProvider>,
    storage: Arc<DynamicStorage>,
    stats: Arc<BrokerStats>,
    resource_monitor: Arc<ResourceMonitor>,
}

impl WasiBroker {
    /// Create a new broker with the given configuration.
    ///
    /// # Panics
    /// Panics if the internal config lock is poisoned.
    #[must_use]
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(wasi_config: &WasiBrokerConfig) -> Self {
        let allow_anonymous = wasi_config.allow_anonymous;
        let config = Arc::new(RwLock::new(wasi_config.to_broker_config()));

        let storage = Arc::new(DynamicStorage::Memory(MemoryBackend::new()));

        let broker_config = config.read().unwrap();
        let base_router = MessageRouter::with_storage(Arc::clone(&storage));
        let with_echo = if broker_config.echo_suppression_config.enabled {
            base_router.with_echo_suppression_key(
                broker_config.echo_suppression_config.property_key.clone(),
            )
        } else {
            base_router
        };
        let with_handler = if let Some(ref handler) = broker_config.event_handler {
            with_echo.with_event_handler(Arc::clone(handler))
        } else {
            with_echo
        };
        let router = if broker_config.max_outbound_rate_per_client > 0 {
            Arc::new(
                with_handler.with_max_outbound_rate(broker_config.max_outbound_rate_per_client),
            )
        } else {
            Arc::new(with_handler)
        };
        drop(broker_config);

        let auth_provider: Arc<dyn AuthProvider> = if allow_anonymous {
            Arc::new(AllowAllAuthProvider)
        } else {
            let password_provider = PasswordAuthProvider::new().with_anonymous(false);
            let acl_manager = AclManager::allow_all();
            Arc::new(ComprehensiveAuthProvider::with_providers(
                password_provider,
                acl_manager,
            ))
        };

        let stats = Arc::new(BrokerStats::new());

        let max_clients = config.read().map(|c| c.max_clients).unwrap_or(1000);
        let limits = ResourceLimits {
            max_connections: max_clients,
            ..Default::default()
        };
        let resource_monitor = Arc::new(ResourceMonitor::new(limits));

        Self {
            config,
            router,
            auth_provider,
            storage,
            stats,
            resource_monitor,
        }
    }

    /// Create a new broker with default configuration (anonymous access enabled).
    #[must_use]
    pub fn default_anonymous() -> Self {
        Self::new(&WasiBrokerConfig::default())
    }

    /// Run the broker, accepting TCP connections on the given address.
    ///
    /// This consumes the broker and blocks the calling thread on
    /// [`wstd::runtime::block_on`] forever (or until the process is killed).
    pub fn run(self, bind_addr: &str) {
        let owned_addr = bind_addr.to_string();
        block_on(self.serve(owned_addr));
    }

    /// Async entrypoint that drives the accept loop.
    ///
    /// Use this when you want to compose the broker with other async work
    /// inside an existing `wstd::runtime::block_on` call instead of letting
    /// [`WasiBroker::run`] start the runtime for you.
    pub async fn serve(self, bind_addr: String) {
        let listener = match TcpListener::bind(bind_addr.as_str()).await {
            Ok(l) => l,
            Err(e) => {
                error!(bind_addr = %bind_addr, error = %e, "Failed to bind listener");
                return;
            }
        };
        info!(bind_addr = %bind_addr, "WASI MQTT broker listening");

        let config = self.config;
        let router = self.router;
        let auth_provider = self.auth_provider;
        let storage = self.storage;
        let stats = self.stats;
        let resource_monitor = self.resource_monitor;

        let mut incoming = listener.incoming();
        while let Some(client) = incoming.next().await {
            let client_socket = match client {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Accept failed");
                    continue;
                }
            };

            // wstd's `TcpStream::peer_addr` returns a `String` rather than
            // a `SocketAddr`, so we use it for logging only and fall back to
            // an unspecified address for the auth provider input.
            let peer = client_socket
                .peer_addr()
                .unwrap_or_else(|_| "<unknown>".to_string());
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

            info!(peer = %peer, "New connection");

            let stream = WasiStream::new(client_socket);
            let handler = WasiClientHandler::new(
                addr,
                Arc::clone(&config),
                Arc::clone(&router),
                Arc::clone(&auth_provider),
                Arc::clone(&storage),
                Arc::clone(&stats),
                Arc::clone(&resource_monitor),
            );

            spawn(async move {
                if let Err(e) = handler.run(stream).await {
                    error!(%addr, error = %e, "Client handler error");
                }
            })
            .detach();
        }
    }

    /// Get a reference to the message router.
    #[must_use]
    pub fn router(&self) -> &Arc<MessageRouter> {
        &self.router
    }

    /// Get a reference to the broker stats.
    #[must_use]
    pub fn stats(&self) -> &Arc<BrokerStats> {
        &self.stats
    }
}
