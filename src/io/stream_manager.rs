use std::collections::HashMap;

use crate::core::error::BluefinError;

use super::Result;

#[derive(Debug)]
pub struct StreamBuffer {
    /// The data's associated segment numebr
    pub segment_number: u64,
    /// The data yielded by the segment
    pub data: Vec<u8>,
}

/// Maximum 'window' or number of segments we can keep buffered.
const MAXIMUM_WINDOW_SIZE: u64 = 100;

/// Iterator for currently available, in-ordered buffered data stored in `StreamManagerEntry`
pub struct Iter(Vec<StreamBuffer>);

impl IntoIterator for Iter {
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
}

impl StreamManagerEntry {
    pub(crate) fn new(expected: u64) -> Self {
        Self {
            expected,
            buffer: HashMap::new(),
        }
    }

    #[inline]
    pub(crate) fn insert(&mut self, buffer: StreamBuffer) -> Result<()> {
        // Segment is not within the current window, cannot insert.
        if buffer.segment_number >= self.expected + MAXIMUM_WINDOW_SIZE
            || buffer.segment_number < self.expected
        {
            return Err(BluefinError::UnexpectedSegmentError);
        }

        let _ = self.buffer.insert(buffer.segment_number, buffer);

        Ok(())
    }

    #[inline]
    fn into_iter(&mut self) -> Iter {
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

        Iter(v)
    }
}

#[derive(Debug)]
/// Manages stream IO buffering. Streams require a separate manager from the connection-based
/// `ConnectionManager` as streams are bytestreams and require a different flow control.
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
