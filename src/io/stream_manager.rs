use std::{collections::HashMap, task::Waker};

use crate::{
    core::{error::BluefinError, header::PacketType, packet::Packet},
    set_waker,
};

use super::{Buffer, Result};

#[derive(Debug)]
pub struct StreamBuffer {
    /// The data's associated segment numebr
    pub segment_number: u64,
    /// The data yielded by the segment
    pub data: Vec<u8>,
}

/// Maximum 'window' or number of segments we can keep buffered.
const MAXIMUM_WINDOW_SIZE: u64 = 100;

/// Iterator for consumed currently available, in-ordered buffered data stored in `StreamManagerEntry`
pub struct ConsumedIter(Vec<StreamBuffer>);

impl IntoIterator for ConsumedIter {
    type Item = StreamBuffer;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[derive(Debug)]
pub(crate) struct StreamManagerBuffer {
    /// The smallest sequence number that we are expecting to have non-empty data for. This means that each
    /// read action should yield buffered data from `expected` and onwards (contiguously).
    pub(crate) expected: u64,
    /// Buffered contents for the stream. Key is the segment number and the value is the segment's `StreamBuffer`
    pub(crate) buffer: HashMap<u64, StreamBuffer>,
    /// Waker for the current stream entry
    pub(crate) waker: Option<Waker>,
}

impl StreamManagerBuffer {
    pub(crate) fn new(expected: u64) -> Self {
        Self {
            expected,
            buffer: HashMap::new(),
            waker: None,
        }
    }

    /// Get an iterator over the present buffered data (if any), in order. Because this consumes the buffered data,
    /// therefore the next `expected` segment number is updated.
    #[inline]
    pub(crate) fn into_iter(&mut self) -> ConsumedIter {
        let mut v = vec![];
        let curr_expected = self.expected;

        for num in curr_expected..curr_expected + MAXIMUM_WINDOW_SIZE {
            match self.buffer.remove(&num) {
                Some(buf) => {
                    self.expected = buf.segment_number + 1;
                    v.push(buf);
                }
                None => break,
            }
        }

        ConsumedIter(v)
    }
}

impl Buffer for StreamManagerBuffer {
    type BufferData = StreamBuffer;
    type ConsumedData = ConsumedIter;

    #[inline]
    fn add(&mut self, data: Self::BufferData) -> Result<()> {
        // Segment is not within the current window, cannot insert.
        if data.segment_number >= self.expected + MAXIMUM_WINDOW_SIZE
            || data.segment_number < self.expected
        {
            return Err(BluefinError::UnexpectedSegmentError);
        }

        let _ = self.buffer.insert(data.segment_number, data);

        Ok(())
    }

    #[inline]
    fn consume(&mut self) -> Option<Self::ConsumedData> {
        Some(self.into_iter())
    }

    set_waker!();
}

/// Manages stream IO buffering. Streams require a separate manager from the connection-based
/// `ConnectionManager` as streams are bytestreams and require a different flow control.
#[derive(Debug)]
pub(crate) struct StreamManager {
    /// Key: stream id (14 bits), value: buffered values for the stream
    stream_map: HashMap<u16, StreamManagerBuffer>,
}

impl StreamManager {
    pub(crate) fn new() -> Self {
        Self {
            stream_map: HashMap::new(),
        }
    }

    /// Registers a new stream into the stream manager. This readies an empty buffer in the manager
    /// provided that the `stream_id` has not already been registered. If so, then this function
    /// returns an error. Else, the newly created stream buffer will be expecting to receive
    /// packets with packet number `expected` and above (up to the window maximum)
    pub(crate) fn register_new_stream(&mut self, stream_id: u16, expected: u64) -> Result<()> {
        if self.stream_map.contains_key(&stream_id) {
            return Err(BluefinError::StreamAlreadyExists);
        }

        let buf = StreamManagerBuffer::new(expected);
        let _ = self.stream_map.insert(stream_id, buf);

        Ok(())
    }

    /// Tries to buffer the `packet` into the stream buffer. If the packet number is not within
    /// the buffer's current acceptance window then the data is not buffered and the packet is
    /// dropped.
    pub(crate) fn buffer_to_existing_stream(&mut self, packet: Packet) -> Result<()> {
        let header = packet.payload.header;
        let payload = packet.payload.payload;

        // We're not going to do anything if the payload is empty
        if payload.is_empty() {
            return Ok(());
        }

        // We can only buffer stream packets
        if header.type_field != PacketType::Stream {
            return Err(BluefinError::CannotBufferError(
                "Cannot buffer non-stream packet into stream manager".to_string(),
            ));
        }

        // Stream id is the last 14 bits of the type_specific_payload
        let stream_id = header.type_specific_payload & 0x3fff;

        // Stream must already exist
        if !self.stream_map.contains_key(&stream_id) {
            return Err(BluefinError::NoSuchStreamError);
        }

        let buf_data = StreamBuffer {
            segment_number: header.packet_number,
            data: payload,
        };

        let entry = self.stream_map.get_mut(&stream_id).unwrap();
        entry.add(buf_data)?;

        Ok(())
    }
}
