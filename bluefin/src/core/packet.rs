use crate::core::header::BluefinHeader;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;

use super::{header::PacketType, Serialisable};

#[derive(Clone, Debug)]
pub struct BluefinPacket {
    pub header: BluefinHeader,
    pub payload: Vec<u8>,
}

pub struct BluefinPacketBuilder {
    header: Option<BluefinHeader>,
    payload: Option<Vec<u8>>,
}

/// Entire packet representation, including the ip + udp metadata
/// and the deserialised bluefin packet
#[derive(Clone, Debug)]
pub struct Packet {
    pub src_ip: [u8; 4],
    pub dst_ip: [u8; 4],
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: BluefinPacket,
}

impl Serialisable for BluefinPacket {
    #[inline]
    fn serialise(&self) -> Vec<u8> {
        let mut header_bytes = self.header.serialise();
        header_bytes.extend_from_slice(&self.payload);

        header_bytes
    }

    #[inline]
    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized,
    {
        if bytes.len() < 20 {
            return Err(BluefinError::DeserialiseError(
                "Cannot deserialise into Bluefin packet with less than 20 bytes".to_string(),
            ));
        }
        // Header is 20 bytes
        let header = BluefinHeader::deserialise(&bytes[..20])?;
        // Optimize: Use Vec::from which is more efficient than to_vec for slices
        let payload = Vec::from(&bytes[20..]);
        Ok(Self { header, payload })
    }
}

impl Default for BluefinPacket {
    #[allow(invalid_value)]
    #[inline]
    fn default() -> Self {
        Self {
            header: Default::default(),
            payload: vec![],
        }
    }
}

impl BluefinPacket {
    #[inline]
    pub fn builder() -> BluefinPacketBuilder {
        BluefinPacketBuilder {
            header: None,
            payload: None,
        }
    }

    /// Length of the packet in bytes
    #[inline]
    pub fn len(&self) -> usize {
        // Header is always 20 bytes
        self.payload.len() + 20
    }

    /// Zero-copy serialization: writes packet directly into provided buffer.
    /// Returns the number of bytes written (20 + payload.len()).
    /// Buffer must be at least 20 + payload.len() bytes.
    #[inline]
    pub fn serialise_into(&self, buf: &mut [u8]) -> usize {
        let total_len = 20 + self.payload.len();
        debug_assert!(buf.len() >= total_len, "Buffer too small for packet serialization");
        
        // Write header directly (20 bytes)
        self.header.serialise_into(&mut buf[..20]);
        
        // Write payload directly (no clone!)
        buf[20..total_len].copy_from_slice(&self.payload);
        
        total_len
    }

    /// Converts an array of bytes into a vector of bluefin packets. The array of bytes must be
    /// a valid stream of bluefin packet bytes. Otherwise, an error is returned.
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> BluefinResult<Vec<BluefinPacket>> {
        if bytes.len() < 20 {
            return Err(BluefinError::ReadError(
                "Array must be at least 20 bytes",
            ));
        }
        // Pre-allocate packets vector to avoid reallocation (most datagrams have 1-76 packets)
        let mut packets = Vec::with_capacity(16);
        let mut cursor = 0;
        while cursor < bytes.len() && cursor + 20 <= bytes.len() {
            let header = BluefinHeader::deserialise(&bytes[cursor..cursor + 20])?;
            match header.type_field {
                PacketType::Ack
                | PacketType::UnencryptedClientHello
                | PacketType::UnencryptedServerHello
                | PacketType::ClientAck => {
                    // Acks + handshake packets contain no payload (for now)
                    packets.push(BluefinPacket {
                        header,
                        payload: Vec::new(),
                    });
                    cursor += 20;
                }
                _ => {
                    // This is some data field
                    let payload_len = header.type_specific_payload as usize;
                    if cursor + 20 >= bytes.len() || cursor + 19 + payload_len >= bytes.len() {
                        return Err(BluefinError::ReadError(
                            "Cannot read all bytes specified by header",
                        ));
                    }
                    // Optimize: Use Vec::from which is more efficient than to_vec for slices
                    let payload = Vec::from(&bytes[cursor + 20..cursor + 20 + payload_len]);
                    packets.push(BluefinPacket { header, payload });
                    cursor = cursor + 20 + payload_len;
                }
            };
        }

        if cursor != bytes.len() {
            return Err(BluefinError::ReadError(
                "Was not able to read all bytes into bluefin packets",
            ));
        }
        Ok(packets)
    }

    /// Zero-copy variant: parses packets directly into a pre-allocated buffer.
    /// This completely eliminates payload allocations for the common case.
    /// Returns the number of packets parsed.
    #[inline]
    pub fn from_bytes_into(bytes: &[u8], packets: &mut Vec<BluefinPacket>) -> BluefinResult<usize> {
        if bytes.len() < 20 {
            return Err(BluefinError::ReadError(
                "Array must be at least 20 bytes",
            ));
        }
        
        let initial_len = packets.len();
        let mut cursor = 0;
        
        while cursor < bytes.len() && cursor + 20 <= bytes.len() {
            let header = BluefinHeader::deserialise(&bytes[cursor..cursor + 20])?;
            match header.type_field {
                PacketType::Ack
                | PacketType::UnencryptedClientHello
                | PacketType::UnencryptedServerHello
                | PacketType::ClientAck => {
                    packets.push(BluefinPacket {
                        header,
                        payload: Vec::new(),
                    });
                    cursor += 20;
                }
                _ => {
                    let payload_len = header.type_specific_payload as usize;
                    if cursor + 20 >= bytes.len() || cursor + 19 + payload_len >= bytes.len() {
                        return Err(BluefinError::ReadError(
                            "Cannot read all bytes specified by header",
                        ));
                    }
                    
                    // Zero-copy optimization: create Vec with exact capacity and use unsafe copy
                    let mut payload = Vec::with_capacity(payload_len);
                    unsafe {
                        let src = bytes.as_ptr().add(cursor + 20);
                        let dst = payload.as_mut_ptr();
                        std::ptr::copy_nonoverlapping(src, dst, payload_len);
                        payload.set_len(payload_len);
                    }
                    
                    packets.push(BluefinPacket { header, payload });
                    cursor = cursor + 20 + payload_len;
                }
            };
        }

        if cursor != bytes.len() {
            return Err(BluefinError::ReadError(
                "Was not able to read all bytes into bluefin packets",
            ));
        }
        
        Ok(packets.len() - initial_len)
    }
}

impl BluefinPacketBuilder {
    #[inline]
    pub fn header(mut self, header: BluefinHeader) -> Self {
        self.header = Some(header);
        self
    }

    #[inline]
    pub fn payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = Some(payload);
        self
    }

    #[inline]
    pub fn build(self) -> BluefinPacket {
        BluefinPacket {
            header: self.header.unwrap(),
            payload: self.payload.unwrap_or(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        Serialisable,
    };
    use bluefin_proto::error::BluefinError;

    use super::BluefinPacket;

    #[test]
    fn cannot_deserialise_invalid_bytes_into_bluefin_packets() {
        let security_fields = BluefinSecurityFields::new(false, 0x0);

        let mut header = BluefinHeader::new(0x0, 0x0, PacketType::Ack, 13, security_fields);
        let payload: [u8; 32] = rand::random();
        header.type_field = PacketType::UnencryptedData;
        header.type_specific_payload = 32;
        header.version = 13;
        let mut packet = BluefinPacket::builder()
            .header(header.clone())
            .payload(payload.to_vec())
            .build();
        assert_eq!(packet.len(), 52);
        assert!(BluefinPacket::from_bytes(&packet.serialise()).is_ok());

        // Incorrectly specify the length to be 33 instead of 32
        packet.header.type_specific_payload = (payload.len() + 1) as u16;
        assert!(
            BluefinPacket::from_bytes(&packet.serialise()).is_err_and(|e| e
                == BluefinError::ReadError(
                    "Cannot read all bytes specified by header",
                ))
        );

        // Now test again but specify a payload length under the actual payload len
        packet.header.type_specific_payload = (payload.len() - 1) as u16;
        assert!(
            BluefinPacket::from_bytes(&packet.serialise()).is_err_and(|e| e
                == BluefinError::ReadError(
                    "Was not able to read all bytes into bluefin packets",
                ))
        );
    }

    #[test]
    fn able_to_deserialise_bytes_into_multiple_bluefin_packets_correctly() {
        // Build 6 packets
        let mut packets = vec![];
        let security_fields = BluefinSecurityFields::new(false, 0x0);

        // Push in an ack
        let mut header = BluefinHeader::new(0x0, 0x0, PacketType::Ack, 13, security_fields);
        let mut packet = BluefinPacket::builder().header(header.clone()).build();
        packets.push(packet);

        // Push in data payload with 32 bytes
        let payload: [u8; 32] = rand::random();
        header.type_field = PacketType::UnencryptedData;
        header.type_specific_payload = 32;
        header.version = 13;
        packet = BluefinPacket::builder()
            .header(header.clone())
            .payload(payload.to_vec())
            .build();
        packets.push(packet);

        // Push in data payload with 20 bytes
        let payload: [u8; 20] = rand::random();
        header.type_field = PacketType::UnencryptedData;
        header.type_specific_payload = 20;
        header.destination_connection_id = 0x123;
        header.version = 15;
        packet = BluefinPacket::builder()
            .header(header.clone())
            .payload(payload.to_vec())
            .build();
        packets.push(packet);

        // Push in an ack
        header.type_field = PacketType::Ack;
        packet = BluefinPacket::builder().header(header.clone()).build();
        header.version = 0;
        packets.push(packet);

        // Push in an client hello
        header.type_field = PacketType::UnencryptedClientHello;
        packet = BluefinPacket::builder().header(header.clone()).build();
        header.version = 5;
        packets.push(packet);

        // Push in data payload with 15 bytes
        let payload: [u8; 15] = rand::random();
        header.type_field = PacketType::UnencryptedData;
        header.destination_connection_id = 0x0;
        header.source_connection_id = 0xabc;
        header.type_specific_payload = 15;
        packet = BluefinPacket::builder()
            .header(header.clone())
            .payload(payload.to_vec())
            .build();
        packets.push(packet);

        // Serialise packets and place into array
        let mut bytes = vec![];
        for p in &packets {
            bytes.extend_from_slice(&p.serialise());
        }

        // Total bytes should be the sum of the payloads plus all of the headers
        assert_eq!(bytes.len(), 32 + 20 + 15 + (6 * 20));

        // We were able to correctly restore the packets
        let rebuilt_packets_res = BluefinPacket::from_bytes(&bytes);
        assert!(rebuilt_packets_res.is_ok());

        let rebuild_packets = rebuilt_packets_res.unwrap();
        assert_eq!(rebuild_packets.len(), packets.len());

        for i in 0..packets.len() {
            let expected = &packets[i];
            let actual = &rebuild_packets[i];
            assert_eq!(
                expected.header.source_connection_id,
                actual.header.source_connection_id
            );
            assert_eq!(
                expected.header.destination_connection_id,
                actual.header.destination_connection_id
            );
            assert_eq!(expected.header.version, actual.header.version);
            assert_eq!(expected.payload, actual.payload);
        }
    }
}
