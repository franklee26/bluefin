use std::fmt;

use crate::{
    core::{error::BluefinError, packet::BluefinPacket},
    utils::common::BluefinResult,
};

const MAX_BUFFER_SIZE: usize = 10;
#[derive(Clone)]
pub(crate) struct OrderedBytes {
    /// The connection id that owns the ordered bytes. Used for debugging.
    conn_id: u32,
    /// Represents the in-ordered buffer of packets. This is a circular buffer.
    packets: [Option<BluefinPacket>; MAX_BUFFER_SIZE],
    /// Pointer to the where the packet with the smallest packet number is buffered
    smallest_packet_number_index: usize,
    /// The packet number of the packet that *should* be buffered at packets[start_index] and
    /// is the smallest next expected packet number
    smallest_packet_number: u64,
}

impl fmt::Display for OrderedBytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut packets_str = "[".to_string();
        for _p in self.packets.as_ref() {
            match _p {
                Some(p) => packets_str.push_str(&format!(
                    "Some(packet_number: {}, payload: {:?})\n",
                    p.header.packet_number, p.payload
                )),
                None => packets_str.push_str("None\n"),
            }
        }
        packets_str.push_str(" ]");
        write!(
            f,
            "(id: {}, start #: {}, start ix: {}, buff: {})",
            self.conn_id,
            self.smallest_packet_number,
            self.smallest_packet_number_index,
            packets_str
        )
    }
}

impl OrderedBytes {
    pub(crate) fn new(conn_id: u32, start_packet_number: u64) -> Self {
        const ARRAY_REPEAT_VALUE: Option<BluefinPacket> = None;
        let packets = [ARRAY_REPEAT_VALUE; MAX_BUFFER_SIZE];
        Self {
            conn_id,
            packets,
            smallest_packet_number_index: 0,
            smallest_packet_number: start_packet_number,
        }
    }

    #[inline]
    pub(crate) fn set_start_packet_number(&mut self, start_packet_number: u64) {
        self.smallest_packet_number = start_packet_number;
    }

    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: &BluefinPacket) -> BluefinResult<()> {
        let packet_num = packet.header.packet_number;

        // We are expecting a packet with packet number >= start_packet_number
        if packet_num < self.smallest_packet_number {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // We received a packet that cannot fit in the buffer
        let offset = (packet_num - self.smallest_packet_number) as usize;
        if offset >= MAX_BUFFER_SIZE {
            return Err(BluefinError::BufferFullError);
        }

        let index = (self.smallest_packet_number_index + offset) % MAX_BUFFER_SIZE;
        // We do not overwrite packets in the buffer.
        if self.packets[index].is_some() {
            return Err(BluefinError::Unexpected(
                format!(
                    "({}) Already buffered in packet with packet number: {} @ index {}. DEBUG: {}",
                    self.conn_id, packet_num, index, self
                )
                .to_string(),
            ));
        }

        self.packets[index] = Some(packet.clone());
        Ok(())
    }

    #[inline]
    pub(crate) fn consume(&mut self) -> BluefinResult<Vec<u8>> {
        // We are still waiting for a packet
        if self.packets[self.smallest_packet_number_index].is_none() {
            return Err(BluefinError::BufferEmptyError);
        }

        let mut bytes = vec![];

        let mut ix = 0;
        let base = self.smallest_packet_number_index;
        while ix < MAX_BUFFER_SIZE && self.packets[(base + ix) % MAX_BUFFER_SIZE].is_some() {
            let packet = self.packets[(base + ix) % MAX_BUFFER_SIZE]
                .as_mut()
                .unwrap();
            let packet_num = packet.header.packet_number;

            bytes.append(&mut packet.payload);
            self.packets[(base + ix) % MAX_BUFFER_SIZE] = None;

            self.smallest_packet_number = packet_num + 1;
            self.smallest_packet_number_index =
                (self.smallest_packet_number_index + 1) % MAX_BUFFER_SIZE;

            ix += 1;
        }

        Ok(bytes)
    }
}
