use crate::core::header::BluefinHeader;

use super::{error::BluefinError, serialisable::Serialisable};
pub struct BluefinPacket {
    pub header: BluefinHeader,
    pub payload: Vec<u8>,
}

pub struct BluefinPacketBuilder {
    header: Option<BluefinHeader>,
    payload: Option<Vec<u8>>,
}

impl Serialisable for BluefinPacket {
    fn serialise(&self) -> Vec<u8> {
        let mut header_bytes = self.header.serialise();
        let mut payload_bytes = self.payload.clone();
        header_bytes.append(&mut payload_bytes);

        header_bytes
    }

    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized,
    {
        // Header is 20 bytes
        let header = BluefinHeader::deserialise(&bytes[..20])?;
        let payload = bytes[20..].to_vec();
        Ok(Self { header, payload })
    }
}

impl BluefinPacket {
    pub fn builder() -> BluefinPacketBuilder {
        BluefinPacketBuilder {
            header: None,
            payload: None,
        }
    }
}

impl BluefinPacketBuilder {
    pub fn header(mut self, header: BluefinHeader) -> Self {
        self.header = Some(header);
        self
    }

    pub fn payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = Some(payload);
        self
    }

    pub fn build(self) -> BluefinPacket {
        BluefinPacket {
            header: self.header.unwrap(),
            payload: self.payload.unwrap_or(vec![]),
        }
    }
}