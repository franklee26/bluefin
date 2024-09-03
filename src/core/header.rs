use crate::utils::common::BluefinResult;

use super::{error::BluefinError, Serialisable};

/// 4 bits reserved for PacketType => 16 possible packet types
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    UnencryptedClientHello = 0x00,
    UnencryptedServerHello = 0x01,
    Ack = 0x02,
    UnencryptedData = 0x03,
}

impl PacketType {
    fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::UnencryptedClientHello,
            0x01 => Self::UnencryptedServerHello,
            0x02 => Self::Ack,
            0x03 => Self::UnencryptedData,
            _ => panic!("Unknown packet type {}", value),
        }
    }
}

/// This struct contains the encryption flag and header-protection fields for a total of 8 bits
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError> {
        if bytes.len() < 1 {
            return Err(BluefinError::DeserialiseError(
                "Bluefin security fields are one byte".to_string(),
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

/// ```text
/// 0               1               2               3
///  0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |Version|  Type |         Type payload          |E|    Mask     |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                   Source connection id                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                Destination connection id                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                      Packet Number                            |
/// |                                                               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BluefinHeader {
    // The version is 4 bits
    pub version: u8,
    // The type is 4 bits
    pub type_field: PacketType,
    /// type_specific_payload is 16 bits
    pub type_specific_payload: u16,
    /// encryption and mask field total is 8 bits
    pub security_fields: BluefinSecurityFields,
    /// source_connection_id is 32 bits
    pub source_connection_id: u32,
    /// desination_connection_id is 32 bits
    pub destination_connection_id: u32,
    /// packet_number is 64 bits
    pub packet_number: u64,
}

impl BluefinHeader {
    pub fn new(
        source_connection_id: u32,
        destination_connection_id: u32,
        type_field: PacketType,
        type_specific_payload: u16,
        security_fields: BluefinSecurityFields,
    ) -> Self {
        Self {
            version: 0x0,
            type_field,
            type_specific_payload,
            security_fields,
            source_connection_id,
            destination_connection_id,
            packet_number: 0x0,
        }
    }

    #[inline]
    pub fn with_packet_number(&mut self, packet_number: u64) {
        self.packet_number = packet_number;
    }
}

impl Serialisable for BluefinHeader {
    #[inline]
    fn serialise(&self) -> Vec<u8> {
        let first_byte = (self.version << 4) | self.type_field as u8;
        [
            &first_byte.to_be_bytes().as_slice(),
            &self.type_specific_payload.to_be_bytes().as_slice(),
            self.security_fields.serialise().as_slice(),
            &self.source_connection_id.to_be_bytes(),
            &self.destination_connection_id.to_be_bytes(),
            &self.packet_number.to_be_bytes(),
        ]
        .concat()
    }

    #[inline]
    fn deserialise(bytes: &[u8]) -> BluefinResult<Self> {
        if bytes.len() != 20 {
            return Err(BluefinError::DeserialiseError(
                "Bluefin header must be exactly 20 bytes".to_string(),
            ));
        }
        let security_fields = BluefinSecurityFields::deserialise(&[bytes[3]])?;
        Ok(Self {
            version: (bytes[0] & 0xf0) >> 4,
            type_field: PacketType::from_u8(bytes[0] & 0x0f),
            type_specific_payload: u16::from_be_bytes(
                bytes[1..3]
                    .try_into()
                    .expect("type specific payload should be 2 bytes"),
            ),
            security_fields,
            source_connection_id: u32::from_be_bytes(
                bytes[4..8]
                    .try_into()
                    .expect("source connection id should be 4 bytes"),
            ),
            destination_connection_id: u32::from_be_bytes(
                bytes[8..12]
                    .try_into()
                    .expect("destination connection id should be 4 bytes"),
            ),
            packet_number: u64::from_be_bytes(
                bytes[12..20]
                    .try_into()
                    .expect("packet number should be 8 bytes"),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{
        header::{BluefinSecurityFields, PacketType},
        Serialisable,
    };

    use super::BluefinHeader;

    #[test]
    fn bluefin_header_should_serialise_and_deserialise_properly() {
        let security_fields = BluefinSecurityFields::new(true, 0b001_1111);
        let header = BluefinHeader::new(
            0x01020304,
            0x04030201,
            PacketType::UnencryptedServerHello,
            0x1234,
            security_fields,
        );
        assert_eq!(header.version, 0x0);
        assert_eq!(header.type_field, PacketType::UnencryptedServerHello);
        assert_eq!(header.type_specific_payload, 0x1234);

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
