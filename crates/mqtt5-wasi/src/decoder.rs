use crate::transport::WasiStream;
use bytes::Buf;
use mqtt5_protocol::constants::limits::MAX_PACKET_SIZE;
use mqtt5_protocol::error::{MqttError, Result};
use mqtt5_protocol::packet::{FixedHeader, Packet};

/// Read and decode a single MQTT packet from the stream.
///
/// `protocol_version` is required because MQTT 3.1.1 (v4) and MQTT 5.0 (v5)
/// have different wire formats (v5 adds property fields to most packet types).
pub async fn read_packet(stream: &WasiStream, protocol_version: u8) -> Result<Packet> {
    let mut header_buf = vec![0u8; 5];
    let n = stream.read(&mut header_buf).await?;

    if n == 0 {
        return Err(MqttError::ConnectionClosedByPeer);
    }

    let mut cursor = &header_buf[..n];
    let fixed_header = FixedHeader::decode(&mut cursor)?;

    let remaining_length = fixed_header.remaining_length as usize;
    if remaining_length > MAX_PACKET_SIZE as usize {
        return Err(MqttError::PacketTooLarge {
            size: remaining_length,
            max: MAX_PACKET_SIZE as usize,
        });
    }

    let mut body_buf = vec![0u8; remaining_length];

    let from_cursor = cursor.remaining().min(remaining_length);
    if from_cursor > 0 {
        body_buf[..from_cursor].copy_from_slice(&cursor[..from_cursor]);
        cursor.advance(from_cursor);
    }

    // Anything still in cursor belongs to the next packet — return it to the
    // stream so the next call sees it. Without this, packets with
    // `remaining_length == 0` (PINGREQ, PINGRESP, DISCONNECT, AUTH) silently
    // drop the trailing bytes and the decoder desyncs.
    if cursor.has_remaining() {
        stream.pushback(cursor);
    }

    if from_cursor < remaining_length {
        stream.read_exact(&mut body_buf[from_cursor..]).await?;
    }

    let mut body = &body_buf[..];
    Packet::decode_from_body_with_version(
        fixed_header.packet_type,
        &fixed_header,
        &mut body,
        protocol_version,
    )
}
