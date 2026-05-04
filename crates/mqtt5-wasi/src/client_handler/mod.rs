mod connect;
mod lifecycle;
mod publish;
mod subscribe;

use crate::transport::WasiStream;
use bytes::BytesMut;
use mqtt5::broker::auth::AuthProvider;
use mqtt5::broker::config::BrokerConfig;
use mqtt5::broker::resource_monitor::ResourceMonitor;
use mqtt5::broker::router::MessageRouter;
use mqtt5::broker::storage::{ClientSession, DynamicStorage};
use mqtt5::broker::sys_topics::BrokerStats;
use mqtt5_protocol::error::{MqttError, Result};
use mqtt5_protocol::packet::publish::PublishPacket;
use mqtt5_protocol::packet::MqttPacket;
use mqtt5_protocol::packet::Packet;
use mqtt5_protocol::KeepaliveConfig;
use mqtt5_protocol::QoS;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use tracing::{debug, error, info, warn};
use wstd::runtime::spawn;
use wstd::task::sleep;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum AuthState {
    NotStarted,
    InProgress,
    Completed,
}

pub(super) struct PendingConnect {
    pub connect: mqtt5_protocol::packet::connect::ConnectPacket,
    pub assigned_client_id: Option<String>,
}

pub struct WasiClientHandler {
    handler_id: u32,
    pub(super) client_id: Option<String>,
    pub(super) user_id: Option<String>,
    pub(super) protocol_version: u8,
    pub(super) config: Arc<RwLock<BrokerConfig>>,
    pub(super) router: Arc<MessageRouter>,
    pub(super) auth_provider: Arc<dyn AuthProvider>,
    pub(super) storage: Arc<DynamicStorage>,
    pub(super) stats: Arc<BrokerStats>,
    pub(super) resource_monitor: Arc<ResourceMonitor>,
    pub(super) session: Option<ClientSession>,
    publish_rx: flume::Receiver<mqtt5::broker::router::RoutableMessage>,
    publish_tx: flume::Sender<mqtt5::broker::router::RoutableMessage>,
    pub(super) inflight_publishes: HashMap<u16, PublishPacket>,
    pub(super) outbound_inflight: Rc<RefCell<HashMap<u16, PublishPacket>>>,
    pub(super) next_packet_id: Rc<Cell<u16>>,
    pub(super) normal_disconnect: bool,
    pub(super) keep_alive: mqtt5::time::Duration,
    pub(super) auth_method: Option<String>,
    pub(super) auth_state: AuthState,
    pub(super) pending_connect: Option<PendingConnect>,
    client_addr: SocketAddr,
}

static HANDLER_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl WasiClientHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client_addr: SocketAddr,
        config: Arc<RwLock<BrokerConfig>>,
        router: Arc<MessageRouter>,
        auth_provider: Arc<dyn AuthProvider>,
        storage: Arc<DynamicStorage>,
        stats: Arc<BrokerStats>,
        resource_monitor: Arc<ResourceMonitor>,
    ) -> Self {
        let (publish_tx, publish_rx) = flume::bounded(10_000);
        let handler_id = HANDLER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Self {
            handler_id,
            client_id: None,
            user_id: None,
            protocol_version: 5,
            config,
            router,
            auth_provider,
            storage,
            stats,
            resource_monitor,
            session: None,
            publish_rx,
            publish_tx,
            inflight_publishes: HashMap::new(),
            outbound_inflight: Rc::new(RefCell::new(HashMap::new())),
            next_packet_id: Rc::new(Cell::new(1)),
            normal_disconnect: false,
            keep_alive: mqtt5::time::Duration::from_secs(60),
            auth_method: None,
            auth_state: AuthState::NotStarted,
            pending_connect: None,
            client_addr,
        }
    }

    pub async fn run(mut self, stream: WasiStream) -> Result<()> {
        let stream = Rc::new(stream);

        self.wait_for_connect(&stream).await?;

        let client_id = self.client_id.clone().unwrap();
        let (disconnect_tx, disconnect_rx) = tokio::sync::oneshot::channel();

        self.router
            .register_client(client_id.clone(), self.publish_tx.clone(), disconnect_tx)
            .await;

        self.stats.client_connected();

        info!(
            handler = self.handler_id,
            client_id = %client_id,
            addr = %self.client_addr,
            "Client connected"
        );

        let result = self.packet_loop(&stream, disconnect_rx).await;

        if !self.normal_disconnect {
            self.publish_will_message(&client_id).await;
        }

        info!(
            handler = self.handler_id,
            client_id = %client_id,
            normal = self.normal_disconnect,
            "Client disconnected"
        );

        self.router.unregister_client(&client_id).await;
        self.stats.client_disconnected();

        result
    }

    async fn packet_loop(
        &mut self,
        stream: &Rc<WasiStream>,
        disconnect_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<()> {
        use crate::decoder::read_packet;

        let running = Rc::new(RefCell::new(true));

        // Watch for session takeover
        {
            let running_clone = Rc::clone(&running);
            spawn(async move {
                match disconnect_rx.await {
                    Ok(()) => {
                        info!("Session takeover signal received");
                        *running_clone.borrow_mut() = false;
                    }
                    Err(_) => {
                        debug!("Disconnect sender dropped");
                    }
                }
            })
            .detach();
        }

        // Spawn publish forwarder: reads from the router's publish channel
        // and writes to the client stream independently of the packet read loop.
        self.spawn_publish_forwarder(&running, stream);

        // Spawn keep-alive checker
        let last_packet_time = Rc::new(Cell::new(mqtt5::time::Instant::now()));
        if !self.keep_alive.is_zero() {
            let timeout_dur = KeepaliveConfig::default().timeout_duration(self.keep_alive);
            let running_clone = Rc::clone(&running);
            let last_time = Rc::clone(&last_packet_time);
            let handler_id = self.handler_id;
            spawn(async move {
                loop {
                    sleep(wstd::time::Duration::from_secs(1)).await;
                    if !*running_clone.borrow() {
                        break;
                    }
                    if last_time.get().elapsed() > timeout_dur {
                        warn!(handler = handler_id, "Keep-alive timeout");
                        *running_clone.borrow_mut() = false;
                        break;
                    }
                }
            })
            .detach();
        }

        loop {
            if !*running.borrow() {
                return Ok(());
            }

            match read_packet(stream, self.protocol_version).await {
                Ok(packet) => {
                    last_packet_time.set(mqtt5::time::Instant::now());
                    self.handle_packet(packet, stream).await?;
                }
                Err(e) => {
                    debug!(handler = self.handler_id, error = %e, "Connection closed");
                    return Ok(());
                }
            }
        }
    }

    fn spawn_publish_forwarder(&mut self, running: &Rc<RefCell<bool>>, stream: &Rc<WasiStream>) {
        let running_fwd = Rc::clone(running);
        let publish_rx = std::mem::replace(&mut self.publish_rx, flume::bounded(1).1);
        let stream_fwd = Rc::clone(stream);
        let outbound_inflight_fwd = Rc::clone(&self.outbound_inflight);
        let next_pid_fwd = Rc::clone(&self.next_packet_id);
        let handler_id = self.handler_id;

        spawn(async move {
            loop {
                if !*running_fwd.borrow() {
                    break;
                }

                let routable = match publish_rx.recv_async().await {
                    Ok(r) => r,
                    Err(flume::RecvError::Disconnected) => break,
                };

                let mut publish = routable.publish;
                if publish.qos != QoS::AtMostOnce {
                    let pid = next_pid_fwd.get();
                    next_pid_fwd.set(if pid == u16::MAX { 1 } else { pid + 1 });
                    publish.packet_id = Some(pid);

                    if publish.qos == QoS::ExactlyOnce {
                        outbound_inflight_fwd
                            .borrow_mut()
                            .insert(pid, publish.clone());
                    }
                }

                if let Err(e) =
                    WasiClientHandler::write_publish_packet(&publish, &stream_fwd).await
                {
                    error!(handler = handler_id, error = %e, "Publish forward error");
                    break;
                }
            }
        })
        .detach();
    }

    async fn handle_packet(&mut self, packet: Packet, stream: &WasiStream) -> Result<()> {
        match packet {
            Packet::Subscribe(subscribe) => self.handle_subscribe(subscribe, stream).await,
            Packet::Unsubscribe(unsubscribe) => self.handle_unsubscribe(unsubscribe, stream).await,
            Packet::Publish(publish) => self.handle_publish(publish, stream).await,
            Packet::PubAck(ref puback) => {
                self.handle_puback(puback);
                Ok(())
            }
            Packet::PubRec(pubrec) => self.handle_pubrec(&pubrec, stream).await,
            Packet::PubRel(pubrel) => self.handle_pubrel(pubrel, stream).await,
            Packet::PubComp(ref pubcomp) => {
                self.handle_pubcomp(pubcomp).await;
                Ok(())
            }
            Packet::PingReq => self.handle_pingreq(stream).await,
            Packet::Disconnect(ref disconnect) => self.handle_disconnect(disconnect),
            Packet::Auth(auth) => self.handle_auth(auth, stream).await,
            _ => {
                warn!(handler = self.handler_id, "Unexpected packet type");
                Ok(())
            }
        }
    }

    #[allow(clippy::unused_self)]
    pub(super) async fn write_packet(&self, packet: &Packet, stream: &WasiStream) -> Result<()> {
        let mut buf = BytesMut::new();

        match packet {
            Packet::ConnAck(p) => p.encode(&mut buf)?,
            Packet::SubAck(p) => p.encode(&mut buf)?,
            Packet::UnsubAck(p) => p.encode(&mut buf)?,
            Packet::Publish(p) => p.encode(&mut buf)?,
            Packet::PubAck(p) => p.encode(&mut buf)?,
            Packet::PubRec(p) => p.encode(&mut buf)?,
            Packet::PubRel(p) => p.encode(&mut buf)?,
            Packet::PubComp(p) => p.encode(&mut buf)?,
            Packet::PingResp => {
                mqtt5_protocol::packet::pingresp::PingRespPacket::default().encode(&mut buf)?;
            }
            Packet::Disconnect(p) => p.encode(&mut buf)?,
            Packet::Auth(p) => p.encode(&mut buf)?,
            _ => {
                return Err(MqttError::ProtocolError(format!(
                    "Encoding not implemented for packet type: {packet:?}"
                )));
            }
        }

        stream.write(&buf).await?;
        Ok(())
    }

    pub(super) fn advance_packet_id_past_inflight(&self) {
        let outbound = self.outbound_inflight.borrow();
        let mut candidate = self.next_packet_id.get();
        for _ in 0..u16::MAX {
            if !outbound.contains_key(&candidate)
                && !self.inflight_publishes.contains_key(&candidate)
            {
                self.next_packet_id.set(candidate);
                return;
            }
            candidate = if candidate == u16::MAX {
                1
            } else {
                candidate + 1
            };
        }
    }
}
