use bytes::BytesMut;
use mqtt5::broker::events::{ClientPublishEvent, PublishAction};
use mqtt5::broker::storage::{InflightDirection, InflightMessage, InflightPhase, StorageBackend};
use mqtt5_protocol::error::Result;
use mqtt5_protocol::packet::puback::PubAckPacket;
use mqtt5_protocol::packet::pubcomp::PubCompPacket;
use mqtt5_protocol::packet::publish::PublishPacket;
use mqtt5_protocol::packet::pubrec::PubRecPacket;
use mqtt5_protocol::packet::pubrel::PubRelPacket;
use mqtt5_protocol::packet::MqttPacket;
use mqtt5_protocol::packet::Packet;
use mqtt5_protocol::protocol::v5::reason_codes::ReasonCode;
use mqtt5_protocol::{PropertyId, PropertyValue, QoS};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::transport::WasiStream;

use super::WasiClientHandler;

impl WasiClientHandler {
    pub(super) async fn handle_publish(
        &mut self,
        mut publish: PublishPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        let client_id = self.client_id.as_ref().unwrap();
        self.stats.publish_received(publish.payload.len());

        let authorized = self
            .auth_provider
            .authorize_publish(client_id, self.user_id.as_deref(), &publish.topic_name)
            .await;

        if !authorized {
            warn!(
                "Client {client_id} not authorized to publish to {}",
                publish.topic_name
            );
            self.reject_publish(&publish, ReasonCode::NotAuthorized, stream).await?;
            return Ok(());
        }

        let max_qos = self.config.read().map_or_else(
            |_| {
                warn!("Config read failed for max_qos, using default 2");
                2
            },
            |c| c.maximum_qos,
        );
        if (publish.qos as u8) > max_qos {
            debug!(
                "Client {client_id} sent QoS {} but max is {max_qos}",
                publish.qos as u8
            );
            self.reject_publish(&publish, ReasonCode::QoSNotSupported, stream).await?;
            return Ok(());
        }

        let payload_size = publish.payload.len();
        if !self
            .resource_monitor
            .can_send_message(client_id, payload_size)
            .await
        {
            warn!("Client {client_id} exceeded quota");
            self.reject_publish(&publish, ReasonCode::QuotaExceeded, stream).await?;
            return Ok(());
        }

        publish.properties.inject_sender(self.user_id.as_deref());
        publish
            .properties
            .inject_client_id(Some(client_id.as_str()));

        let client_id = client_id.clone();
        match self.fire_publish_event(&publish, &client_id).await {
            PublishAction::Continue => {
                self.route_publish_by_qos(publish, &client_id, stream).await
            }
            PublishAction::Transform(modified) => {
                self.route_publish_by_qos(modified, &client_id, stream).await
            }
            PublishAction::Handled => self.complete_qos_handshake(publish, stream).await,
        }
    }

    async fn route_publish_by_qos(
        &mut self,
        publish: PublishPacket,
        client_id: &str,
        stream: &WasiStream,
    ) -> Result<()> {
        match publish.qos {
            QoS::AtMostOnce => {
                self.router.route_message(&publish, Some(client_id)).await;
            }
            QoS::AtLeastOnce => {
                self.router.route_message(&publish, Some(client_id)).await;
                let puback = PubAckPacket::new(publish.packet_id.unwrap());
                self.write_packet(&Packet::PubAck(puback), stream).await?;
            }
            QoS::ExactlyOnce => {
                let packet_id = publish.packet_id.unwrap();
                let inflight = InflightMessage::from_publish(
                    &publish,
                    client_id.to_string(),
                    InflightDirection::Inbound,
                    InflightPhase::AwaitingPubrel,
                );
                if let Err(e) = self.storage.store_inflight_message(inflight).await {
                    debug!("failed to persist inbound inflight {packet_id}: {e}");
                }
                self.inflight_publishes.insert(packet_id, publish);
                let pubrec = PubRecPacket::new(packet_id);
                self.write_packet(&Packet::PubRec(pubrec), stream).await?;
            }
        }
        Ok(())
    }

    /// Acknowledge the QoS handshake for a publish whose `PublishAction` was
    /// `Handled`. The application has already consumed it, so it must not be
    /// routed to subscribers.
    async fn complete_qos_handshake(
        &mut self,
        publish: PublishPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        match publish.qos {
            QoS::AtMostOnce => {}
            QoS::AtLeastOnce => {
                let puback = PubAckPacket::new(publish.packet_id.unwrap());
                self.write_packet(&Packet::PubAck(puback), stream).await?;
            }
            QoS::ExactlyOnce => {
                let packet_id = publish.packet_id.unwrap();
                self.inflight_handled.insert(packet_id);
                let pubrec = PubRecPacket::new(packet_id);
                self.write_packet(&Packet::PubRec(pubrec), stream).await?;
            }
        }
        Ok(())
    }

    async fn fire_publish_event(
        &self,
        publish: &PublishPacket,
        client_id: &str,
    ) -> PublishAction {
        let handler = {
            let cfg = match self.config.read() {
                Ok(c) => c,
                Err(_) => {
                    warn!("BrokerConfig RwLock poisoned; skipping event handler");
                    return PublishAction::Continue;
                }
            };
            match cfg.event_handler.as_ref() {
                Some(h) => Arc::clone(h),
                None => return PublishAction::Continue,
            }
        };

        let response_topic = publish
            .properties
            .get(PropertyId::ResponseTopic)
            .and_then(|v| {
                if let PropertyValue::Utf8String(s) = v {
                    Some(Arc::from(s.as_str()))
                } else {
                    None
                }
            });
        let correlation_data = publish
            .properties
            .get(PropertyId::CorrelationData)
            .and_then(|v| {
                if let PropertyValue::BinaryData(b) = v {
                    Some(b.clone())
                } else {
                    None
                }
            });

        let event = ClientPublishEvent {
            client_id: client_id.to_string().into(),
            user_id: self.user_id.as_deref().map(Arc::from),
            topic: publish.topic_name.clone().into(),
            payload: publish.payload.clone(),
            qos: publish.qos,
            retain: publish.retain,
            packet_id: publish.packet_id,
            response_topic,
            correlation_data,
        };
        handler.on_client_publish(event).await
    }

    async fn reject_publish(
        &self,
        publish: &PublishPacket,
        reason_code: ReasonCode,
        stream: &WasiStream,
    ) -> Result<()> {
        if let Some(packet_id) = publish.packet_id {
            match publish.qos {
                QoS::AtLeastOnce => {
                    let mut puback = PubAckPacket::new(packet_id);
                    puback.reason_code = reason_code;
                    self.write_packet(&Packet::PubAck(puback), stream).await?;
                }
                QoS::ExactlyOnce => {
                    let mut pubrec = PubRecPacket::new(packet_id);
                    pubrec.reason_code = reason_code;
                    self.write_packet(&Packet::PubRec(pubrec), stream).await?;
                }
                QoS::AtMostOnce => {}
            }
        }
        Ok(())
    }

    pub(super) fn handle_puback(&mut self, puback: &PubAckPacket) {
        self.outbound_inflight
            .borrow_mut()
            .remove(&puback.packet_id);
    }

    #[allow(clippy::similar_names)]
    pub(super) async fn handle_pubrec(
        &self,
        pubrec: &PubRecPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        let inflight = self.client_id.as_ref().and_then(|client_id| {
            self.outbound_inflight
                .borrow()
                .get(&pubrec.packet_id)
                .map(|publish| {
                    InflightMessage::from_publish(
                        publish,
                        client_id.clone(),
                        InflightDirection::Outbound,
                        InflightPhase::AwaitingPubcomp,
                    )
                })
        });
        if let Some(inflight) = inflight {
            if let Err(e) = self.storage.store_inflight_message(inflight).await {
                debug!(
                    "failed to update inflight phase for {}: {e}",
                    pubrec.packet_id
                );
            }
        }
        let pubrel = PubRelPacket::new(pubrec.packet_id);
        self.write_packet(&Packet::PubRel(pubrel), stream).await?;
        Ok(())
    }

    pub(super) async fn handle_pubrel(
        &mut self,
        pubrel: PubRelPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        let reason_code = if let Some(publish) = self.inflight_publishes.remove(&pubrel.packet_id) {
            let client_id = self.client_id.as_ref().unwrap();
            let _ = self
                .storage
                .remove_inflight_message(client_id, pubrel.packet_id, InflightDirection::Inbound)
                .await;
            self.router.route_message(&publish, Some(client_id)).await;
            ReasonCode::Success
        } else if self.inflight_handled.remove(&pubrel.packet_id) {
            // The publish was consumed via PublishAction::Handled — finish the
            // QoS-2 exchange without re-routing to subscribers.
            ReasonCode::Success
        } else {
            ReasonCode::PacketIdentifierNotFound
        };

        let pubcomp = PubCompPacket::new_with_reason(pubrel.packet_id, reason_code);
        self.write_packet(&Packet::PubComp(pubcomp), stream).await?;
        Ok(())
    }

    pub(super) async fn handle_pubcomp(&mut self, pubcomp: &PubCompPacket) {
        self.outbound_inflight
            .borrow_mut()
            .remove(&pubcomp.packet_id);
        if let Some(client_id) = self.client_id.as_ref() {
            let _ = self
                .storage
                .remove_inflight_message(client_id, pubcomp.packet_id, InflightDirection::Outbound)
                .await;
        }
    }

    pub(super) async fn resend_inflight_messages(&mut self, stream: &WasiStream) -> Result<()> {
        let client_id = self.client_id.as_ref().unwrap();
        let inflights = match self.storage.get_inflight_messages(client_id).await {
            Ok(msgs) => msgs,
            Err(e) => {
                warn!("failed to load inflight messages for {client_id}: {e}");
                return Ok(());
            }
        };

        for msg in inflights {
            match msg.direction {
                InflightDirection::Outbound => match msg.phase {
                    InflightPhase::AwaitingPubrec => {
                        let mut publish = msg.to_publish_packet();
                        publish.dup = true;
                        self.outbound_inflight
                            .borrow_mut()
                            .insert(msg.packet_id, publish.clone());
                        self.write_packet(&Packet::Publish(publish), stream).await?;
                    }
                    InflightPhase::AwaitingPubcomp => {
                        let publish = msg.to_publish_packet();
                        self.outbound_inflight
                            .borrow_mut()
                            .insert(msg.packet_id, publish);
                        let pubrel = PubRelPacket::new(msg.packet_id);
                        self.write_packet(&Packet::PubRel(pubrel), stream).await?;
                    }
                    InflightPhase::AwaitingPubrel => {}
                },
                InflightDirection::Inbound => {
                    let publish = msg.to_publish_packet();
                    self.inflight_publishes.insert(msg.packet_id, publish);
                }
            }
        }
        Ok(())
    }

    pub(super) async fn send_publish(
        &self,
        publish: PublishPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        self.write_packet(&Packet::Publish(publish), stream).await
    }

    pub(super) async fn write_publish_packet(
        publish: &PublishPacket,
        stream: &WasiStream,
    ) -> Result<()> {
        let mut buf = BytesMut::new();
        publish.encode(&mut buf)?;
        stream.write(&buf).await?;
        Ok(())
    }
}
