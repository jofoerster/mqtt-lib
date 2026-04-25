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
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tracing::{error, info};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::network::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress};
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::tcp_create_socket::create_tcp_socket;

/// Standalone WASI MQTT v5.0 broker.
///
/// Uses the native `wasi:sockets/tcp` API with non-blocking
/// `InputStream::read()` for cooperative concurrency.
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
        let router = if broker_config.max_outbound_rate_per_client > 0 {
            Arc::new(with_echo.with_max_outbound_rate(broker_config.max_outbound_rate_per_client))
        } else {
            Arc::new(with_echo)
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
    /// This consumes the broker and blocks forever (or until the process is killed).
    pub fn run(self, bind_addr: &str) {
        let listener = bind_and_listen(bind_addr);
        info!(bind_addr, "WASI MQTT broker listening");

        let config = self.config;
        let router = self.router;
        let auth_provider = self.auth_provider;
        let storage = self.storage;
        let stats = self.stats;
        let resource_monitor = self.resource_monitor;

        crate::executor::block_on(async move {
            loop {
                if !listener.subscribe().ready() {
                    crate::executor::yield_now().await;
                    continue;
                }

                match listener.accept() {
                    Ok((client_socket, input, output)) => {
                        let addr = client_socket.remote_address().map_or(
                            SocketAddr::new(
                                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                                0,
                            ),
                            |a| socket_addr_from_wasi(&a),
                        );

                        info!(%addr, "New connection");

                        let stream = WasiStream::new(client_socket, input, output);
                        let handler = WasiClientHandler::new(
                            addr,
                            Arc::clone(&config),
                            Arc::clone(&router),
                            Arc::clone(&auth_provider),
                            Arc::clone(&storage),
                            Arc::clone(&stats),
                            Arc::clone(&resource_monitor),
                        );

                        crate::executor::spawn(async move {
                            if let Err(e) = handler.run(stream).await {
                                error!(%addr, error = %e, "Client handler error");
                            }
                        });
                    }
                    Err(wasi::sockets::network::ErrorCode::WouldBlock) => {
                        crate::executor::yield_now().await;
                    }
                    Err(e) => {
                        error!(error = ?e, "Accept failed");
                        crate::executor::yield_now().await;
                    }
                }
            }
        });
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

fn parse_bind_addr(addr: &str) -> IpSocketAddress {
    let sock: SocketAddr = addr.parse().expect("Invalid bind address");
    match sock {
        SocketAddr::V4(v4) => {
            let octets = v4.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: v4.port(),
                address: (octets[0], octets[1], octets[2], octets[3]),
            })
        }
        SocketAddr::V6(_) => {
            panic!("IPv6 not yet supported in WASI broker");
        }
    }
}

fn socket_addr_from_wasi(addr: &IpSocketAddress) -> SocketAddr {
    match addr {
        IpSocketAddress::Ipv4(v4) => {
            let ip =
                std::net::Ipv4Addr::new(v4.address.0, v4.address.1, v4.address.2, v4.address.3);
            SocketAddr::new(std::net::IpAddr::V4(ip), v4.port)
        }
        IpSocketAddress::Ipv6(v6) => {
            let ip = std::net::Ipv6Addr::new(
                v6.address.0,
                v6.address.1,
                v6.address.2,
                v6.address.3,
                v6.address.4,
                v6.address.5,
                v6.address.6,
                v6.address.7,
            );
            SocketAddr::new(std::net::IpAddr::V6(ip), v6.port)
        }
    }
}

fn bind_and_listen(addr: &str) -> TcpSocket {
    let network = instance_network();
    let bind_addr = parse_bind_addr(addr);

    let family = match &bind_addr {
        IpSocketAddress::Ipv4(_) => IpAddressFamily::Ipv4,
        IpSocketAddress::Ipv6(_) => IpAddressFamily::Ipv6,
    };

    let socket = create_tcp_socket(family).expect("Failed to create TCP socket");

    socket
        .start_bind(&network, bind_addr)
        .expect("Failed to start bind");
    wasi::io::poll::poll(&[&socket.subscribe()]);
    socket.finish_bind().expect("Failed to finish bind");

    socket.start_listen().expect("Failed to start listen");
    wasi::io::poll::poll(&[&socket.subscribe()]);
    socket.finish_listen().expect("Failed to finish listen");

    socket
}
