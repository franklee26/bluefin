use std::{collections::HashMap, task::Waker};

use crate::{core::error::BluefinError, set_waker};

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
pub(crate) struct StreamManagerEntry {
    /// The smallest sequence number that we are expecting to have non-empty data for. This means that each
    /// read action should yield buffered data from `expected` and onwards (contiguously).
    pub(crate) expected: u64,
    /// Buffered contents for the stream. Key is the segment number and the value is the segment's `StreamBuffer`
    pub(crate) buffer: HashMap<u64, StreamBuffer>,
    /// Waker for the current stream entry
    pub(crate) waker: Option<Waker>,
}

impl StreamManagerEntry {
    pub(crate) fn new(expected: u64) -> Self {
        Self {
            expected,
            buffer: HashMap::new(),
            waker: None,
        }
    }

    /// Get an iterator over the present buffered data (if any), in order
    #[inline]
    pub(crate) fn into_iter(&mut self) -> ConsumedIter {
        let mut v = vec![];
        let mut last_segment_num = self.expected;

        for num in self.expected..self.expected + MAXIMUM_WINDOW_SIZE {
            match self.buffer.remove(&num) {
                Some(buf) => {
                    last_segment_num = buf.segment_number;
                    v.push(buf);
                }
                None => break,
            }
        }

        self.expected = last_segment_num;

        ConsumedIter(v)
    }
}

impl Buffer for StreamManagerEntry {
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
    stream_map: HashMap<u64, StreamManagerEntry>,
}

impl StreamManager {
    pub(crate) fn new() -> Self {
        Self {
            stream_map: HashMap::new(),
        }
    }
}
