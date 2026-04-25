use bytes::Bytes;
use mqtt5_protocol::error::Result;
use mqtt5_protocol::packet::puback::PubAckPacket;
use mqtt5_protocol::packet::pubcomp::PubCompPacket;
use mqtt5_protocol::packet::publish::PublishPacket;
use mqtt5_protocol::packet::pubrec::PubRecPacket;
use mqtt5_protocol::packet::pubrel::PubRelPacket;
use mqtt5_protocol::packet::Packet;
use mqtt5_protocol::QoS;
use tracing::debug;

use super::WasiClient;

impl WasiClient {
    /// Publish a message. For `QoS` 1 / 2 the broker's acknowledgement is
    /// processed in [`WasiClient::recv`] — call it in a loop to drive the
    /// `QoS` handshake.
    ///
    /// # Errors
    /// Returns an error if encoding or writing the PUBLISH fails.
    pub async fn publish(
        &mut self,
        topic: impl Into<String>,
        payload: impl Into<Bytes>,
        qos: QoS,
    ) -> Result<()> {
        self.publish_with_retain(topic, payload, qos, false).await
    }

    /// Publish a message with the retain flag set.
    ///
    /// # Errors
    /// Returns an error if encoding or writing the `PUBLISH` fails.
    #[allow(clippy::unused_async)]
    pub async fn publish_with_retain(
        &mut self,
        topic: impl Into<String>,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
    ) -> Result<()> {
        let mut publish = if self.protocol_version == 4 {
            PublishPacket::new_v311(topic, payload, qos)
        } else {
            PublishPacket::new(topic, payload, qos)
        };
        publish.retain = retain;

        if qos != QoS::AtMostOnce {
            let pid = self.next_packet_id();
            publish.packet_id = Some(pid);
            self.outbound_inflight.insert(pid, qos);
        }

        self.write(&Packet::Publish(publish))
    }

    /// Handles a `PUBLISH` received from the broker.
    /// Returns `Some(publish)` for messages the user should observe; `None`
    /// when the message is being held pending `PUBREL` (`QoS` 2).
    pub(super) fn handle_inbound_publish(
        &mut self,
        publish: PublishPacket,
    ) -> Result<Option<PublishPacket>> {
        match publish.qos {
            QoS::AtMostOnce => Ok(Some(publish)),
            QoS::AtLeastOnce => {
                let packet_id = publish
                    .packet_id
                    .ok_or_else(|| missing_packet_id("PUBLISH QoS 1"))?;
                let puback = PubAckPacket::new(packet_id);
                self.write(&Packet::PubAck(puback))?;
                Ok(Some(publish))
            }
            QoS::ExactlyOnce => {
                let packet_id = publish
                    .packet_id
                    .ok_or_else(|| missing_packet_id("PUBLISH QoS 2"))?;
                self.inbound_qos2.insert(packet_id, publish);
                let pubrec = PubRecPacket::new(packet_id);
                self.write(&Packet::PubRec(pubrec))?;
                Ok(None)
            }
        }
    }

    /// Outbound `QoS` 2: broker accepted `PUBLISH`; reply with `PUBREL`.
    #[allow(clippy::similar_names)]
    pub(super) fn handle_pubrec(&self, pubrec: &PubRecPacket) -> Result<()> {
        let pubrel = PubRelPacket::new(pubrec.packet_id);
        self.write(&Packet::PubRel(pubrel))
    }

    /// Inbound `QoS` 2: broker confirmed our `PUBREC`; release the held publish.
    pub(super) fn handle_pubrel(
        &mut self,
        pubrel: &PubRelPacket,
    ) -> Result<Option<PublishPacket>> {
        let publish = self.inbound_qos2.remove(&pubrel.packet_id);
        let pubcomp = PubCompPacket::new(pubrel.packet_id);
        self.write(&Packet::PubComp(pubcomp))?;
        if publish.is_none() {
            debug!(packet_id = pubrel.packet_id, "PUBREL for unknown packet id");
        }
        Ok(publish)
    }
}

fn missing_packet_id(context: &str) -> mqtt5_protocol::error::MqttError {
    mqtt5_protocol::error::MqttError::ProtocolError(format!(
        "{context} missing packet identifier"
    ))
}
