use crate::{connection::connection::Connection, core::header::BluefinHeader};

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

    /// Validates incoming packet bytes against the connection, sets/updates relevant header
    /// fields and returns the deserialised Bluefin packet, if possible. This func should
    /// be invoked as part of every incoming read operation EXCEPT the very first
    /// handshake operations. This is becasue in the handshake, we are still setting
    /// the correct src/dst connection id's and other context metadata.
    ///
    /// Otherwise this func errors.
    pub fn validate(&self, conn: &mut Connection) -> Result<(), BluefinError> {
        let header = self.header;

        // Validate the header
        // Verify that the connection id is correct (the packet's dst conn id should be our src id)
        if header.destination_connection_id != conn.source_id {
            return Err(BluefinError::InvalidHeaderError(format!(
                "Expecting incoming packet's dst conn id ({:#08x}) to be equal to ({:#08x})",
                header.destination_connection_id, conn.source_id
            )));
        }

        // Verify that the packet's src conn id is our expected dst conn id
        if header.source_connection_id != conn.dest_id {
            return Err(BluefinError::InvalidHeaderError(format!(
                "Expected packet w/ conn id {}, but found {} instead.",
                conn.dest_id, header.source_connection_id
            )));
        }

        // Verify that the packet number is as expected
        if header.packet_number != conn.context.packet_number + 1 {
            return Err(BluefinError::InvalidHeaderError(format!(
                "Received packet number {:#016x}, was expecting {:#016x}",
                header.packet_number,
                conn.context.packet_number + 1
            )));
        }

        // Bluefin payloads must be at most 1492 bytes
        if self.payload.len() > 1492 {
            return Err(BluefinError::LargePayloadError(
                self.payload.len().to_string(),
            ));
        }

        // Increment packet number
        conn.context.packet_number += 2;

        Ok(())
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
