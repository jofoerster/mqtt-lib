use crate::client_handler::WasiClientHandler;
use crate::transport::WasiStream;
use mqtt5::broker::acl::AclManager;
use mqtt5::broker::auth::{
    AllowAllAuthProvider, AuthProvider, ComprehensiveAuthProvider, PasswordAuthProvider,
};
use mqtt5::broker::config::{BrokerConfig, ChangeOnlyDeliveryConfig, LoadBalancerConfig};
use mqtt5::broker::resource_monitor::{ResourceLimits, ResourceMonitor};
use mqtt5::broker::router::MessageRouter;
use mqtt5::broker::storage::{DynamicStorage, MemoryBackend};
use mqtt5::broker::sys_topics::BrokerStats;
use mqtt5::time::Duration;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tracing::{error, info};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::network::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress};
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::tcp_create_socket::create_tcp_socket;

/// Configuration for the WASI MQTT broker.
#[allow(clippy::struct_excessive_bools)]
pub struct WasiBrokerConfig {
    pub max_clients: usize,
    pub session_expiry_interval_secs: u32,
    pub max_packet_size: usize,
    pub topic_alias_maximum: u16,
    pub retain_available: bool,
    pub maximum_qos: u8,
    pub wildcard_subscription_available: bool,
    pub subscription_identifier_available: bool,
    pub shared_subscription_available: bool,
    pub server_keep_alive_secs: Option<u32>,
    pub allow_anonymous: bool,
    pub change_only_delivery_enabled: bool,
    pub change_only_delivery_patterns: Vec<String>,
    pub echo_suppression_enabled: bool,
    pub echo_suppression_property_key: Option<String>,
    pub max_outbound_rate_per_client: u32,
    pub load_balancer_backends: Vec<String>,
}

impl Default for WasiBrokerConfig {
    fn default() -> Self {
        Self {
            max_clients: 1000,
            session_expiry_interval_secs: 3600,
            max_packet_size: 268_435_456,
            topic_alias_maximum: 65535,
            retain_available: true,
            maximum_qos: 2,
            wildcard_subscription_available: true,
            subscription_identifier_available: true,
            shared_subscription_available: true,
            server_keep_alive_secs: None,
            allow_anonymous: true,
            change_only_delivery_enabled: false,
            change_only_delivery_patterns: Vec::new(),
            echo_suppression_enabled: false,
            echo_suppression_property_key: None,
            max_outbound_rate_per_client: 0,
            load_balancer_backends: Vec::new(),
        }
    }
}

impl WasiBrokerConfig {
    fn to_broker_config(&self) -> BrokerConfig {
        BrokerConfig {
            max_clients: self.max_clients,
            session_expiry_interval: Duration::from_secs(u64::from(
                self.session_expiry_interval_secs,
            )),
            max_packet_size: self.max_packet_size,
            topic_alias_maximum: self.topic_alias_maximum,
            retain_available: self.retain_available,
            maximum_qos: self.maximum_qos,
            wildcard_subscription_available: self.wildcard_subscription_available,
            subscription_identifier_available: self.subscription_identifier_available,
            shared_subscription_available: self.shared_subscription_available,
            server_keep_alive: self
                .server_keep_alive_secs
                .map(|s| Duration::from_secs(u64::from(s))),
            change_only_delivery_config: ChangeOnlyDeliveryConfig {
                enabled: self.change_only_delivery_enabled,
                topic_patterns: self.change_only_delivery_patterns.clone(),
            },
            echo_suppression_config: mqtt5::broker::config::EchoSuppressionConfig {
                enabled: self.echo_suppression_enabled,
                property_key: self
                    .echo_suppression_property_key
                    .clone()
                    .unwrap_or_else(|| "x-origin-client-id".to_string()),
            },
            max_outbound_rate_per_client: self.max_outbound_rate_per_client,
            load_balancer: if self.load_balancer_backends.is_empty() {
                None
            } else {
                Some(LoadBalancerConfig::new(self.load_balancer_backends.clone()))
            },
            ..Default::default()
        }
    }
}

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
                        let addr = client_socket
                            .remote_address()
                            .map_or(
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
            let ip = std::net::Ipv4Addr::new(
                v4.address.0,
                v4.address.1,
                v4.address.2,
                v4.address.3,
            );
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
