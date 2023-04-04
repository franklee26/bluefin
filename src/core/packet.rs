use crate::{core::header::BluefinHeader, network::connection::Connection};

use super::{error::BluefinError, header::BluefinStreamHeader, serialisable::Serialisable};

pub struct BluefinStreamPacket {
    pub header: BluefinHeader,
    pub stream_header: BluefinStreamHeader,
    pub payload: Vec<u8>,
}

pub struct BluefinStreamPacketBuilder {
    header: Option<BluefinHeader>,
    stream_header: Option<BluefinStreamHeader>,
    payload: Option<Vec<u8>>,
}

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
pub(crate) struct Packet {
    pub(crate) src_ip: [u8; 4],
    pub(crate) dst_ip: [u8; 4],
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) payload: BluefinPacket,
}

impl BluefinStreamPacket {
    pub(crate) fn builder() -> BluefinStreamPacketBuilder {
        BluefinStreamPacketBuilder {
            header: None,
            stream_header: None,
            payload: None,
        }
    }
}

impl BluefinStreamPacketBuilder {
    pub(crate) fn header(mut self, header: BluefinHeader) -> Self {
        self.header = Some(header);
        self
    }

    pub(crate) fn stream_header(mut self, stream_header: BluefinStreamHeader) -> Self {
        self.stream_header = Some(stream_header);
        self
    }

    pub fn payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = Some(payload);
        self
    }

    pub fn build(self) -> BluefinStreamPacket {
        BluefinStreamPacket {
            header: self.header.unwrap(),
            stream_header: self.stream_header.unwrap(),
            payload: self.payload.unwrap_or(vec![]),
        }
    }
}

impl Serialisable for BluefinStreamPacket {
    fn serialise(&self) -> Vec<u8> {
        let mut header_bytes = self.header.serialise();
        let mut stream_header_bytes = self.stream_header.serialise();
        header_bytes.append(&mut stream_header_bytes);
        header_bytes.append(&mut self.payload.clone());

        header_bytes
    }

    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized,
    {
        // Header is 20 bytes
        let header = BluefinHeader::deserialise(&bytes[..20])?;
        // Stream header is 4 bytes
        let stream_header = BluefinStreamHeader::deserialise(&bytes[20..24])?;
        let payload = bytes[24..].to_vec();
        Ok(Self {
            header,
            stream_header,
            payload,
        })
    }
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
