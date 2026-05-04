use mqtt5_protocol::error::{MqttError, Result};
use mqtt5_protocol::packet::suback::SubAckReasonCode;
use mqtt5_protocol::packet::subscribe::{SubscribePacket, TopicFilter};
use mqtt5_protocol::packet::unsuback::UnsubAckReasonCode;
use mqtt5_protocol::packet::unsubscribe::UnsubscribePacket;
use mqtt5_protocol::packet::Packet;
use mqtt5_protocol::QoS;
use tracing::debug;

use super::WasiClient;

impl WasiClient {
    /// Subscribe to a single topic filter. Sends a SUBSCRIBE and reads
    /// packets until the matching SUBACK arrives.
    ///
    /// # Errors
    /// Returns an error if the broker rejects the subscription or the
    /// connection fails.
    pub async fn subscribe(&mut self, topic_filter: impl Into<String>, qos: QoS) -> Result<QoS> {
        let codes = self.subscribe_many(vec![(topic_filter.into(), qos)]).await?;
        codes
            .into_iter()
            .next()
            .ok_or_else(|| MqttError::ProtocolError("SUBACK had no reason codes".to_string()))?
            .granted_qos()
            .ok_or_else(|| MqttError::ProtocolError("Subscription not granted".to_string()))
    }

    /// Subscribe to multiple topic filters in a single SUBSCRIBE packet.
    ///
    /// # Errors
    /// Returns an error if writing fails or the SUBACK is malformed.
    pub async fn subscribe_many(
        &mut self,
        filters: Vec<(String, QoS)>,
    ) -> Result<Vec<SubAckReasonCode>> {
        let packet_id = self.next_packet_id();
        let mut subscribe = if self.protocol_version == 4 {
            SubscribePacket::new_v311(packet_id)
        } else {
            SubscribePacket::new(packet_id)
        };
        subscribe.filters = filters
            .into_iter()
            .map(|(filter, qos)| TopicFilter::new(filter, qos))
            .collect();

        self.write(&Packet::Subscribe(subscribe)).await?;
        debug!(packet_id, "Sent SUBSCRIBE");

        self.read_until(|p| match p {
            Packet::SubAck(suback) if suback.packet_id == packet_id => {
                Some(suback.reason_codes.clone())
            }
            _ => None,
        })
        .await
    }

    /// Unsubscribe from a single topic filter.
    ///
    /// # Errors
    /// Returns an error if writing fails or the UNSUBACK is malformed.
    pub async fn unsubscribe(&mut self, topic_filter: impl Into<String>) -> Result<()> {
        self.unsubscribe_many(vec![topic_filter.into()]).await?;
        Ok(())
    }

    /// Unsubscribe from multiple topic filters in a single UNSUBSCRIBE packet.
    ///
    /// # Errors
    /// Returns an error if writing fails or the UNSUBACK is malformed.
    pub async fn unsubscribe_many(
        &mut self,
        filters: Vec<String>,
    ) -> Result<Vec<UnsubAckReasonCode>> {
        let packet_id = self.next_packet_id();
        let mut unsubscribe = if self.protocol_version == 4 {
            UnsubscribePacket::new_v311(packet_id)
        } else {
            UnsubscribePacket::new(packet_id)
        };
        unsubscribe.filters = filters;

        self.write(&Packet::Unsubscribe(unsubscribe)).await?;
        debug!(packet_id, "Sent UNSUBSCRIBE");

        self.read_until(|p| match p {
            Packet::UnsubAck(unsuback) if unsuback.packet_id == packet_id => {
                Some(unsuback.reason_codes.clone())
            }
            _ => None,
        })
        .await
    }
}
