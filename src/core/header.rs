use super::serialisable::{DeserialiseError, Serialisable};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    UnencryptedHandshake = 0x00,
    Data = 0x01,
    Error = 0x02,
    Warning = 0x03,
    DiscoveryProbe = 0x04,
}

impl PacketType {
    fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::UnencryptedHandshake,
            0x01 => Self::Data,
            0x02 => Self::Error,
            0x03 => Self::Warning,
            0x04 => Self::DiscoveryProbe,
            _ => panic!("Unknown packet type {}", value),
        }
    }
}

/// This struct contains both the packet type and type-specific fields. Together, these
/// two fields are 2 one byte.
#[derive(Debug, PartialEq, Eq)]
pub struct BluefinTypeFields {
    /// The packet type is 4 bits
    packet_type: PacketType,
    /// Packet-type specific payload is 12 bits
    type_specific_payload: u16,
}

impl BluefinTypeFields {
    pub fn new(packet_type: PacketType, type_specific_payload: u16) -> Self {
        // Ensure that the packet_type and payload have the correct size
        // packet_type is 4 bits
        if packet_type as u8 > 15 {
            panic!("packet_type field cannot be longer than 4 bits");
        }
        // type_specific_payload is 12 bits 0b0000 0000 0000
        if type_specific_payload > 4095 {
            panic!("type_specific_payload field cannot be longer than 12 bits");
        }
        Self {
            packet_type,
            type_specific_payload,
        }
    }
}

impl Serialisable for BluefinTypeFields {
    #[inline]
    fn serialise(&self) -> Vec<u8> {
        let mut two_bytes: u16 = 0x0;
        two_bytes |= self.packet_type as u16;
        two_bytes <<= 12;
        two_bytes |= self.type_specific_payload;
        vec![
            ((two_bytes & 0xff00) >> 8) as u8,
            (two_bytes & 0x00ff) as u8,
        ]
    }

    #[inline]
    fn deserialise(bytes: &[u8]) -> Result<Self, DeserialiseError> {
        if bytes.len() < 2 {
            return Err(DeserialiseError::new("Bluefin type fields are two bytes"));
        }
        // Two bytes. First 4 bits is the packet_type then the remaining 12 bits are the type-specific payload
        let packet_type = (bytes[0] & 0xf0) >> 4;
        let type_specific_payload: u16 = (((bytes[0] & 0x0f) as u16) << 8) | (bytes[1] as u16);
        Ok(Self {
            packet_type: PacketType::from_u8(packet_type),
            type_specific_payload,
        })
    }
}

/// This struct contains the encryption flag and header-protection fields for a total of 8 bits
#[derive(Debug, PartialEq, Eq)]
pub struct BluefinSecurityFields {
    /// header_encrypted is one bit and signals whether the header contains encrypted fields
    header_encrypted: bool,
    /// the mask used in header protection and is 7 bits
    header_protection_mask: u8,
}

impl BluefinSecurityFields {
    pub fn new(header_encrypted: bool, header_protection_mask: u8) -> Self {
        if header_protection_mask > 127 {
            panic!("header_protection_mask field cannot be longer than 127 bits");
        }
        Self {
            header_encrypted,
            header_protection_mask,
        }
    }
}

impl Serialisable for BluefinSecurityFields {
    fn serialise(&self) -> Vec<u8> {
        let mut byte: u8 = 0x0;
        if self.header_encrypted {
            byte |= 0x80;
        }
        byte |= self.header_protection_mask;
        vec![byte]
    }

    fn deserialise(bytes: &[u8]) -> Result<Self, DeserialiseError> {
        if bytes.len() < 1 {
            return Err(DeserialiseError::new(
                "Bluefin security fields are one byte",
            ));
        }
        let byte = bytes[0];
        let header_encrypted: bool = ((byte & 0x80) >> 7) != 0;
        let header_protection_mask: u8 = byte & 0x7f;
        Ok(Self {
            header_encrypted,
            header_protection_mask,
        })
    }
}

/**
 *  0               1               2               3
 *  0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
 * +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 * |    Version    |  Type |     Type payload      |E|    Mask     |
 * +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 * |                   Source connection id                        |
 * +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 * |                Destination connection id                      |
 * +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 * |                      Packet Number                            |
 * |                                                               |
 * +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 */
#[derive(Debug, PartialEq, Eq)]
pub struct BluefinHeader {
    // The version is 8 bits
    pub version: u8,
    /// type and type_specific_payload total is 16 bits
    pub type_and_type_specific_payload: BluefinTypeFields,
    /// encryption and mask field total is 8 bits
    pub security_fields: BluefinSecurityFields,
    /// source_connection_id is 32 bits
    pub source_connection_id: [u8; 4],
    /// desination_connection_id is 32 bits
    pub destination_connection_id: [u8; 4],
    /// packet_number is 64 bits
    pub packet_number: [u8; 8],
}

impl BluefinHeader {
    pub fn new(
        source_connection_id: [u8; 4],
        destination_connection_id: [u8; 4],
        type_and_type_specific_payload: BluefinTypeFields,
        security_fields: BluefinSecurityFields,
    ) -> Self {
        Self {
            version: 0x0,
            type_and_type_specific_payload,
            security_fields,
            source_connection_id,
            destination_connection_id,
            packet_number: [0x00; 8],
        }
    }

    pub fn with_packet_number(&mut self, packet_number: [u8; 8]) {
        self.packet_number = packet_number;
    }
}

impl Serialisable for BluefinHeader {
    fn serialise(&self) -> Vec<u8> {
        [
            [self.version].as_slice(),
            self.type_and_type_specific_payload.serialise().as_slice(),
            self.security_fields.serialise().as_slice(),
            self.source_connection_id.as_slice(),
            self.destination_connection_id.as_slice(),
            self.packet_number.as_slice(),
        ]
        .concat()
    }

    fn deserialise(bytes: &[u8]) -> Result<Self, DeserialiseError> {
        if bytes.len() < 20 {
            return Err(DeserialiseError::new("Bluefin header is 20 bytes"));
        }
        let type_and_type_specific_payload = BluefinTypeFields::deserialise(&bytes[1..3])?;
        let security_fields = BluefinSecurityFields::deserialise(&[bytes[3]])?;
        Ok(Self {
            version: bytes[0].try_into().expect("version is 1 byte"),
            type_and_type_specific_payload,
            security_fields,
            source_connection_id: bytes[4..8]
                .try_into()
                .expect("source connection id should be 4 bytes"),
            destination_connection_id: bytes[8..12]
                .try_into()
                .expect("destination connection id should be 4 bytes"),
            packet_number: bytes[12..20]
                .try_into()
                .expect("packet number should be 8 bytes"),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{
        header::{BluefinSecurityFields, PacketType},
        serialisable::Serialisable,
    };

    use super::{BluefinHeader, BluefinTypeFields};

    #[test]
    fn type_and_type_specific_payload_should_serialise_and_deserialise_correctly() {
        let fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0b0001_0011_0111);
        assert_eq!(fields.packet_type, PacketType::UnencryptedHandshake);
        assert_eq!(fields.type_specific_payload, 0b0001_0011_0111);
        let serialised = fields.serialise();
        // The byte stream would look like (in network order) 0b0000 0001 0011 0111
        assert_eq!(serialised.len(), 2);
        assert_eq!(serialised[0], 0b0000_0001);
        assert_eq!(serialised[1], 0b0011_0111);

        let deserialised = BluefinTypeFields::deserialise(&serialised);
        match deserialised {
            Ok(d_field) => assert_eq!(d_field, fields),
            Err(_) => assert!(false),
        }
    }

    #[test]
    #[should_panic(expected = "type_specific_payload field cannot be longer than 12 bits")]
    fn type_and_type_specific_payload_should_panic_if_payload_is_out_of_bounds() {
        let _ = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0b1_0000_0000_0000);
    }

    #[test]
    fn bluefine_header_should_serialise_and_deserialise_properly() {
        let types = BluefinTypeFields::new(PacketType::Error, 0b0011_0111_1111);
        let security_fields = BluefinSecurityFields::new(true, 0b001_1111);
        let header = BluefinHeader::new(
            [0x01, 0x02, 0x03, 0x04],
            [0x04, 0x03, 0x02, 0x01],
            types,
            security_fields,
        );
        assert_eq!(header.version, 0x0);

        let serialised = header.serialise();
        // Headers are 20 bytes
        assert_eq!(serialised.len(), 20);

        let deserialised = BluefinHeader::deserialise(&serialised);
        match deserialised {
            Ok(d_field) => assert_eq!(d_field, header),
            Err(_) => assert!(false),
        }
    }
}
