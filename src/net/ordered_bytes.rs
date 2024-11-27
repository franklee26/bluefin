use std::fmt;

use crate::{
    core::{error::BluefinError, packet::BluefinPacket},
    utils::common::BluefinResult,
};

/// Represents the maximum number of *packets* we can buffer in memory. When bytes are consumed
/// via [OrderedBytes::consume()], we can only consume at most [MAX_BUFFER_SIZE] number of packets.
pub const MAX_BUFFER_SIZE: usize = 10000000;

/// [OrderedBytes] represents the connection's buffered packets. OrderedBytes stores at most
/// [MAX_BUFFER_SIZE] number of bluefin packets and maintains their intended consumption
/// order based on the packet number. OrderedBytes only stores packet payload information and
/// does not deal with ack packets.
#[derive(Clone)]
pub(crate) struct OrderedBytes {
    /// The connection id that owns the ordered bytes. Used for debugging.
    conn_id: u32,
    /// Represents the in-ordered buffer of packets. This is a circular buffer.
    packets: Box<[Option<BluefinPacket>; MAX_BUFFER_SIZE]>,
    /// Pointer to the where the packet with the smallest packet number is buffered
    smallest_packet_number_index: usize,
    /// The packet number of the packet that *should* be buffered at packets\[start_index\] and
    /// is the smallest next expected packet number
    smallest_packet_number: u64,
    /// Stores any potential carry over bytes from a previous consume. These bytes belong to
    /// a packet we have already consumed.
    carry_over_bytes: Option<Vec<u8>>,
}

/// The result returned when [OrderedBytes are consumed](OrderedBytes::consume()). This result
/// not only includes the ordered-payload bytes but also includes vital information for sending
/// ack packets to the sender. Notice that once bytes are returned in a [ConsumeResult] then
/// the bytes are no longer available in the [OrderedBytes] for consumption.
pub(crate) struct ConsumeResult {
    num_packets_consumed: usize,
    base_packet_number: u64,
    bytes_consumed: u64,
}

impl ConsumeResult {
    #[inline]
    fn new(num_packets_consumed: usize, base_packet_number: u64, bytes_consumed: u64) -> Self {
        Self {
            num_packets_consumed,
            base_packet_number,
            bytes_consumed,
        }
    }

    #[inline]
    pub(crate) fn get_num_packets_consumed(&self) -> usize {
        self.num_packets_consumed
    }

    #[inline]
    pub(crate) fn get_base_packet_number(&self) -> u64 {
        self.base_packet_number
    }

    #[inline]
    pub(crate) fn get_bytes_consumed(&self) -> u64 {
        self.bytes_consumed
    }
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
            "(id: {}, start #: {}, start ix: {}, buff: {}, carry_over: {:?})",
            self.conn_id,
            self.smallest_packet_number,
            self.smallest_packet_number_index,
            packets_str,
            self.carry_over_bytes
        )
    }
}

impl OrderedBytes {
    pub(crate) fn new(conn_id: u32, start_packet_number: u64) -> Self {
        const ARRAY_REPEAT_VALUE: Option<BluefinPacket> = None;
        let packets = vec![ARRAY_REPEAT_VALUE; MAX_BUFFER_SIZE]
            .try_into()
            .unwrap();
        Self {
            conn_id,
            packets,
            smallest_packet_number_index: 0,
            smallest_packet_number: start_packet_number,
            carry_over_bytes: None,
        }
    }

    /// The start packet number represents the smallest packet number that we can buffer
    /// or have buffered in the [OrderedBytes]. For example, if our start packet number
    /// was set to 5 and we can buffer at most 10 packets, then we can buffer up packets
    /// with packet number up to and including 5 + 10 = 15. This also means that we do
    /// not expect nor can we buffer a packet with packet number less than 5.
    #[inline]
    pub(crate) fn set_start_packet_number(&mut self, start_packet_number: u64) {
        self.smallest_packet_number = start_packet_number;
    }

    /// Attempts buffer in the packet. An `Ok(())` indicates a successful in-order buffering.
    /// Else, the packet was unabled to be buffered and the operation can possibly succceed
    /// in the future (for example, the buffer could be currently full).
    ///
    /// If [MAX_BUFFER_SIZE] or more number of packets are already buffered, then we cannot
    /// buffer any more packets and will drop packets from the network.
    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: BluefinPacket) -> BluefinResult<()> {
        let packet_num = packet.header.packet_number;

        // We are expecting a packet with packet number >= start_packet_number
        if packet_num < self.smallest_packet_number {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // We received a packet that cannot fit in the buffer
        let offset = (packet_num - self.smallest_packet_number) as usize;
        if offset >= MAX_BUFFER_SIZE {
            return Err(BluefinError::BufferFullError(
                "Ordered bytes buffer full".to_string(),
            ));
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

        self.packets[index] = Some(packet);
        Ok(())
    }

    /// Ok(()) indicates there are bytes to consume. Error otherwise.
    pub(crate) fn peek(&self) -> BluefinResult<()> {
        // There are at least carry over bytes to consume
        if let Some(_) = self.carry_over_bytes.as_ref() {
            return Ok(());
        }

        // We have at least one packet buffered
        if let Some(_) = self.packets[self.smallest_packet_number_index] {
            return Ok(());
        }

        Err(BluefinError::BufferEmptyError)
    }

    /// Consumes the buffer, which removes consumable bytes from the buffer in-order and places
    /// them in the [ConsumeResult]. Notice that if we cannot fit a packet payload within `len`
    /// bytes, then we fit `len` bytes of the payload into the [ConsumeResult] and place the
    /// excess remaining bytes into [OrderedBytes::carry_over_bytes]. Even though some of the packet's
    /// payload are still unconsumed, we treat as though the packet was consumed in the
    /// [ConsumeResult].
    ///
    /// Otherwise, we place the buffered bytes into the output [ConsumeResult] until we have
    /// either exhausted the buffer or we have consumed `len` bytes. This can consume at most
    /// [MAX_BUFFER_SIZE] number of packets.
    ///
    /// [OrderedBytes::consume()] will return [BluefinError::BufferEmptyError] if no bytes can be
    /// consumed.
    #[inline]
    pub(crate) fn consume(&mut self, len: usize, buf: &mut [u8]) -> BluefinResult<ConsumeResult> {
        let mut num_bytes = 0;
        let mut writer_ix = 0;

        // peek into carry over bytes
        if let Some(ref mut c_bytes) = self.carry_over_bytes {
            // We can take all of the carry over
            if c_bytes.len() <= len {
                num_bytes += c_bytes.len();
                buf[writer_ix..writer_ix + c_bytes.len()].copy_from_slice(c_bytes);
                writer_ix += c_bytes.len();
                self.carry_over_bytes = None;
            // We still have some bytes left over in the carry over...
            } else {
                let drained = c_bytes.drain(len..).collect();
                buf[writer_ix..writer_ix + len].copy_from_slice(&c_bytes);
                self.carry_over_bytes = Some(drained);
                return Ok(ConsumeResult::new(0, 0, len as u64));
            }
        }

        let mut ix = 0;
        let base = self.smallest_packet_number_index;
        let base_packet_number = {
            if let Some(ref _p) = self.packets[base] {
                _p.header.packet_number
            } else {
                0
            }
        };

        while ix < MAX_BUFFER_SIZE
            && self.packets[(base + ix) % MAX_BUFFER_SIZE].is_some()
            && num_bytes < len
        {
            let packet = self.packets[(base + ix) % MAX_BUFFER_SIZE]
                .as_mut()
                .unwrap();
            let packet_num = packet.header.packet_number;
            let bytes_remaining = len - num_bytes;
            let payload_len = packet.payload.len();

            // We cannot return all of the payload. We will partially consume the payload and
            // store the remaining in the carry over
            if payload_len > bytes_remaining {
                buf[writer_ix..writer_ix + bytes_remaining]
                    .copy_from_slice(&packet.payload[..bytes_remaining]);
                writer_ix += bytes_remaining;
                self.carry_over_bytes = Some(packet.payload[bytes_remaining..].to_vec());
                num_bytes += bytes_remaining;
            // We have enough space left to consume the entirity of this buffer
            } else {
                buf[writer_ix..writer_ix + payload_len].copy_from_slice(&packet.payload);
                writer_ix += payload_len;
                num_bytes += payload_len;
            }

            self.packets[(base + ix) % MAX_BUFFER_SIZE] = None;

            self.smallest_packet_number = packet_num + 1;
            self.smallest_packet_number_index =
                (self.smallest_packet_number_index + 1) % MAX_BUFFER_SIZE;

            ix += 1;
        }

        // Nothing to consume, including any potential carry-over bytes
        if num_bytes == 0 {
            return Err(BluefinError::BufferEmptyError);
        }

        Ok(ConsumeResult::new(ix, base_packet_number, num_bytes as u64))
    }
}
