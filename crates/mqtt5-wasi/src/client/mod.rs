mod publish;
mod subscribe;

use crate::config::WasiClientConfig;
use crate::decoder::read_packet;
use crate::transport::WasiStream;
use bytes::BytesMut;
use mqtt5_protocol::error::{MqttError, Result};
use mqtt5_protocol::packet::connect::ConnectPacket;
use mqtt5_protocol::packet::disconnect::DisconnectPacket;
use mqtt5_protocol::packet::pingreq::PingReqPacket;
use mqtt5_protocol::packet::publish::PublishPacket;
use mqtt5_protocol::packet::{MqttPacket, Packet};
use mqtt5_protocol::types::{ConnectOptions, ProtocolVersion};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::ip_name_lookup::resolve_addresses;
use wasi::sockets::network::{IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress};
use wasi::sockets::tcp_create_socket::create_tcp_socket;

/// Standalone WASI MQTT v5.0 / v3.1.1 client.
///
/// Uses the native `wasi:sockets/tcp` API with non-blocking I/O on the same
/// cooperative executor that powers the WASI broker. Reuses the [`WasiStream`]
/// transport and [`read_packet`] decoder so the wire-level behaviour is
/// identical to the broker's connection handling.
pub struct WasiClient {
    stream: Rc<WasiStream>,
    protocol_version: u8,
    next_packet_id: u16,
    /// `QoS` 2 publishes received from the broker, awaiting `PUBREL`.
    inbound_qos2: HashMap<u16, PublishPacket>,
    /// Outbound `QoS` 1/2 packet ids awaiting `PUBACK` / `PUBCOMP`.
    outbound_inflight: HashMap<u16, mqtt5_protocol::QoS>,
    /// Buffered `PUBLISH` packets received while waiting on a `SUBACK` / `UNSUBACK`.
    pending_messages: VecDeque<PublishPacket>,
    last_send: Rc<Cell<Instant>>,
    keepalive_running: Rc<RefCell<bool>>,
}

impl WasiClient {
    /// Connect to a broker at `broker_addr` (`host:port` or `ip:port`) and wait for CONNACK.
    ///
    /// Spawns a background keep-alive task on the executor. Must be called from
    /// inside [`crate::executor::block_on`].
    ///
    /// # Errors
    /// Returns an error if name resolution, the TCP connect, or the CONNACK fails.
    pub async fn connect(broker_addr: &str, config: &WasiClientConfig) -> Result<Self> {
        if !matches!(config.protocol_version, 4 | 5) {
            return Err(MqttError::ProtocolError(format!(
                "Unsupported protocol version: {}",
                config.protocol_version
            )));
        }

        let stream = Rc::new(tcp_connect(broker_addr).await?);

        let connect_packet = build_connect_packet(config);
        write_packet(&Packet::Connect(Box::new(connect_packet)), &stream)?;

        let connack = match read_packet(&stream, config.protocol_version).await? {
            Packet::ConnAck(c) => c,
            other => {
                return Err(MqttError::ProtocolError(format!(
                    "Expected CONNACK, got {}",
                    other.packet_type_name()
                )));
            }
        };

        if !connack.reason_code.is_success() {
            return Err(MqttError::ProtocolError(format!(
                "Broker rejected connection: {:?}",
                connack.reason_code
            )));
        }

        info!(
            client_id = %config.client_id,
            session_present = connack.session_present,
            "Connected to MQTT broker"
        );

        let client = Self {
            stream: Rc::clone(&stream),
            protocol_version: config.protocol_version,
            next_packet_id: 1,
            inbound_qos2: HashMap::new(),
            outbound_inflight: HashMap::new(),
            pending_messages: VecDeque::new(),
            last_send: Rc::new(Cell::new(Instant::now())),
            keepalive_running: Rc::new(RefCell::new(true)),
        };

        client.spawn_keepalive(config.keep_alive);

        Ok(client)
    }

    /// Returns the next received PUBLISH packet, handling all other packet
    /// types (PUBACK / PUBREC / PUBREL / PUBCOMP / PINGRESP) internally.
    ///
    /// Returns `Ok(None)` if the broker sent a DISCONNECT or closed the connection.
    ///
    /// # Errors
    /// Returns an error if a packet decode fails or an unexpected packet arrives.
    pub async fn recv(&mut self) -> Result<Option<PublishPacket>> {
        if let Some(msg) = self.pending_messages.pop_front() {
            return Ok(Some(msg));
        }

        loop {
            match read_packet(&self.stream, self.protocol_version).await {
                Ok(Packet::Publish(publish)) => {
                    if let Some(msg) = self.handle_inbound_publish(publish)? {
                        return Ok(Some(msg));
                    }
                }
                Ok(Packet::PubAck(ref puback)) => {
                    self.outbound_inflight.remove(&puback.packet_id);
                }
                Ok(Packet::PubRec(pubrec)) => self.handle_pubrec(&pubrec)?,
                Ok(Packet::PubRel(pubrel)) => {
                    if let Some(msg) = self.handle_pubrel(&pubrel)? {
                        return Ok(Some(msg));
                    }
                }
                Ok(Packet::PubComp(ref pubcomp)) => {
                    self.outbound_inflight.remove(&pubcomp.packet_id);
                }
                Ok(Packet::PingResp) => debug!("Received PINGRESP"),
                Ok(Packet::Disconnect(d)) => {
                    info!(reason = ?d.reason_code, "Broker sent DISCONNECT");
                    return Ok(None);
                }
                Ok(other) => {
                    warn!("Unexpected packet: {}", other.packet_type_name());
                }
                Err(MqttError::ConnectionClosedByPeer) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Send a DISCONNECT and stop the keep-alive task. Consumes the client.
    ///
    /// # Errors
    /// Returns an error if writing the DISCONNECT packet fails.
    pub fn disconnect(self) -> Result<()> {
        *self.keepalive_running.borrow_mut() = false;
        let disconnect = DisconnectPacket::normal();
        write_packet(&Packet::Disconnect(disconnect), &self.stream)?;
        Ok(())
    }

    pub(super) fn next_packet_id(&mut self) -> u16 {
        let id = self.next_packet_id;
        self.next_packet_id = if id == u16::MAX { 1 } else { id + 1 };
        id
    }

    pub(super) fn write(&self, packet: &Packet) -> Result<()> {
        write_packet(packet, &self.stream)?;
        self.last_send.set(Instant::now());
        Ok(())
    }

    /// Read packets until one matching `matcher` arrives. PUBLISH packets are
    /// buffered for the next [`WasiClient::recv`] call; ack-style packets are
    /// handled in-line.
    pub(super) async fn read_until<F, T>(&mut self, mut matcher: F) -> Result<T>
    where
        F: FnMut(&Packet) -> Option<T>,
    {
        loop {
            let packet = read_packet(&self.stream, self.protocol_version).await?;
            if let Some(value) = matcher(&packet) {
                return Ok(value);
            }
            match packet {
                Packet::Publish(publish) => {
                    if let Some(msg) = self.handle_inbound_publish(publish)? {
                        self.pending_messages.push_back(msg);
                    }
                }
                Packet::PubAck(ref puback) => {
                    self.outbound_inflight.remove(&puback.packet_id);
                }
                Packet::PubRec(pubrec) => self.handle_pubrec(&pubrec)?,
                Packet::PubRel(pubrel) => {
                    if let Some(msg) = self.handle_pubrel(&pubrel)? {
                        self.pending_messages.push_back(msg);
                    }
                }
                Packet::PubComp(ref pubcomp) => {
                    self.outbound_inflight.remove(&pubcomp.packet_id);
                }
                Packet::PingResp => {}
                Packet::Disconnect(d) => {
                    return Err(MqttError::ProtocolError(format!(
                        "Broker disconnected: {:?}",
                        d.reason_code
                    )));
                }
                other => warn!(
                    "Unexpected packet during read_until: {}",
                    other.packet_type_name()
                ),
            }
        }
    }

    fn spawn_keepalive(&self, keep_alive: Duration) {
        if keep_alive.is_zero() {
            return;
        }

        let stream = Rc::clone(&self.stream);
        let last_send = Rc::clone(&self.last_send);
        let running = Rc::clone(&self.keepalive_running);

        crate::executor::spawn(async move {
            loop {
                crate::timer::sleep(Duration::from_secs(1)).await;
                if !*running.borrow() {
                    break;
                }
                if last_send.get().elapsed() < keep_alive {
                    continue;
                }
                let mut buf = Vec::new();
                if PingReqPacket::default().encode(&mut buf).is_err() {
                    break;
                }
                if stream.write(&buf).is_err() {
                    *running.borrow_mut() = false;
                    break;
                }
                last_send.set(Instant::now());
                debug!("Sent PINGREQ");
            }
        });
    }
}

impl Drop for WasiClient {
    fn drop(&mut self) {
        *self.keepalive_running.borrow_mut() = false;
    }
}

pub(super) fn write_packet(packet: &Packet, stream: &WasiStream) -> Result<()> {
    let mut buf = BytesMut::new();
    match packet {
        Packet::Connect(p) => p.encode(&mut buf)?,
        Packet::Subscribe(p) => p.encode(&mut buf)?,
        Packet::Unsubscribe(p) => p.encode(&mut buf)?,
        Packet::Publish(p) => p.encode(&mut buf)?,
        Packet::PubAck(p) => p.encode(&mut buf)?,
        Packet::PubRec(p) => p.encode(&mut buf)?,
        Packet::PubRel(p) => p.encode(&mut buf)?,
        Packet::PubComp(p) => p.encode(&mut buf)?,
        Packet::Disconnect(p) => p.encode(&mut buf)?,
        Packet::Auth(p) => p.encode(&mut buf)?,
        _ => {
            return Err(MqttError::ProtocolError(format!(
                "Encoding not implemented for packet type: {}",
                packet.packet_type_name()
            )));
        }
    }
    stream.write(&buf)
}

fn build_connect_packet(config: &WasiClientConfig) -> ConnectPacket {
    let options = ConnectOptions {
        client_id: config.client_id.clone(),
        keep_alive: mqtt5_protocol::time::Duration::from_secs(config.keep_alive.as_secs()),
        clean_start: config.clean_start,
        username: config.username.clone(),
        password: config.password.clone(),
        will: None,
        properties: mqtt5_protocol::types::ConnectProperties::default(),
        protocol_version: ProtocolVersion::try_from(config.protocol_version).unwrap_or_default(),
    };
    if config.protocol_version == 4 {
        ConnectPacket::new_v311(options)
    } else {
        ConnectPacket::new(options)
    }
}

async fn tcp_connect(addr: &str) -> Result<WasiStream> {
    let (host, port) = parse_host_port(addr)?;

    let network = instance_network();

    let remote = if let Ok(socket_addr) = host.parse::<SocketAddr>() {
        to_wasi_addr(socket_addr)
    } else {
        resolve_host(&network, &host, port).await?
    };

    let family = match &remote {
        IpSocketAddress::Ipv4(_) => IpAddressFamily::Ipv4,
        IpSocketAddress::Ipv6(_) => IpAddressFamily::Ipv6,
    };
    let socket = create_tcp_socket(family)
        .map_err(|e| MqttError::Io(format!("WASI create_tcp_socket error: {e:?}")))?;

    socket
        .start_connect(&network, remote)
        .map_err(|e| MqttError::Io(format!("WASI start_connect error: {e:?}")))?;

    let (input, output) = loop {
        if !socket.subscribe().ready() {
            crate::executor::yield_now().await;
            continue;
        }
        match socket.finish_connect() {
            Ok(streams) => break streams,
            Err(wasi::sockets::network::ErrorCode::WouldBlock) => {
                crate::executor::yield_now().await;
            }
            Err(e) => {
                return Err(MqttError::Io(format!("WASI finish_connect error: {e:?}")));
            }
        }
    };

    Ok(WasiStream::new(socket, input, output))
}

async fn resolve_host(
    network: &wasi::sockets::network::Network,
    host: &str,
    port: u16,
) -> Result<IpSocketAddress> {
    let resolver = resolve_addresses(network, host)
        .map_err(|e| MqttError::Io(format!("WASI resolve_addresses error: {e:?}")))?;
    loop {
        if !resolver.subscribe().ready() {
            crate::executor::yield_now().await;
            continue;
        }
        match resolver.resolve_next_address() {
            Ok(Some(ip)) => return Ok(ip_with_port(ip, port)),
            Ok(None) => {
                return Err(MqttError::Io(format!(
                    "No addresses resolved for host '{host}'"
                )));
            }
            Err(wasi::sockets::network::ErrorCode::WouldBlock) => {
                crate::executor::yield_now().await;
            }
            Err(e) => {
                return Err(MqttError::Io(format!("WASI resolve error: {e:?}")));
            }
        }
    }
}

fn parse_host_port(addr: &str) -> Result<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        // IPv6 in brackets: [::1]:1883
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| MqttError::ProtocolError(format!("Invalid IPv6 address: {addr}")))?;
        let port = port_part
            .parse::<u16>()
            .map_err(|_| MqttError::ProtocolError(format!("Invalid port in {addr}")))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_part) = addr
        .rsplit_once(':')
        .ok_or_else(|| MqttError::ProtocolError(format!("Address must be host:port: {addr}")))?;
    let port = port_part
        .parse::<u16>()
        .map_err(|_| MqttError::ProtocolError(format!("Invalid port in {addr}")))?;
    Ok((host.to_string(), port))
}

fn to_wasi_addr(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(v4) => {
            let o = v4.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: v4.port(),
                address: (o[0], o[1], o[2], o[3]),
            })
        }
        SocketAddr::V6(v6) => {
            let s = v6.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: v6.port(),
                flow_info: 0,
                address: (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
                scope_id: 0,
            })
        }
    }
}

fn ip_with_port(ip: IpAddress, port: u16) -> IpSocketAddress {
    match ip {
        IpAddress::Ipv4(addr) => IpSocketAddress::Ipv4(Ipv4SocketAddress {
            port,
            address: addr,
        }),
        IpAddress::Ipv6(addr) => IpSocketAddress::Ipv6(Ipv6SocketAddress {
            port,
            flow_info: 0,
            address: addr,
            scope_id: 0,
        }),
    }
}
