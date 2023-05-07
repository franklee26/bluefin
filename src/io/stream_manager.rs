use std::{
    collections::{HashMap, VecDeque},
    ops::Deref,
    task::Waker,
    vec,
};

use crate::{
    core::{
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
    },
    set_waker,
};

use super::{Buffer, Result};

/// The buffered stream data
#[derive(Debug)]
pub struct StreamBuffer {
    /// The data's associated segment numebr
    pub segment_number: u64,
    /// The segment; we need the whole segment because this may receive an ack packet
    pub segment: Segment,
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

impl ConsumedIter {
    /// Returns `true` if there are any `StreamBuffer` buffered
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of elements in the iterator
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Debug)]
pub(crate) struct StreamReadBuffer {
    /// The smallest sequence number that we are expecting to have non-empty data for. This means that each
    /// read action should yield buffered data from `expected` and onwards (contiguously).
    pub(crate) expected: u64,
    /// Buffered contents for the stream. Key is the segment number and the value is the segment's `StreamBuffer`
    pub(crate) buffer: HashMap<u64, StreamBuffer>,
    /// Waker for the current stream entry
    pub(crate) waker: Option<Waker>,
}

impl StreamReadBuffer {
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

impl Buffer for StreamReadBuffer {
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
        let iter = self.into_iter();
        if iter.is_empty() {
            return None;
        }

        Some(iter)
    }

    set_waker!();
}

/// Manages stream read buffering. Streams require a separate manager from the connection-based
/// `ConnectionManager` as streams are bytestreams and require a different flow control.
#[derive(Debug)]
pub(crate) struct ReadStreamManager {
    /// Key: stream id (14 bits), value: buffered values for the stream
    stream_map: HashMap<u16, StreamReadBuffer>,
}

/// Type alias for semantics. Segments are packets but with 'squished' data contents.
pub type Segment = BluefinPacket;

/// A segment struct but with the additional `acked` metadata
#[derive(Debug)]
struct WrappedSegment {
    segment: Segment,
    acked: bool,
}

impl WrappedSegment {
    fn new(segment: Segment) -> Self {
        Self {
            segment,
            acked: false,
        }
    }
}

/// Nice-to-have deref trait to retrieve the inner segment
impl Deref for WrappedSegment {
    type Target = Segment;
    fn deref(&self) -> &Self::Target {
        &self.segment
    }
}

/// Buffers ready to-send segments.
/// ```text
/// SegmentBuffer
/// |-----------------------------------------|
/// |                   |                |    |
/// |-----------------------------------------|
/// ^                   ^                ^    ^
/// l                   s                r    e
/// ```
#[derive(Debug)]
struct SegmentBuffer {
    /// Left ptr to the window. This is the smallest segment number that we have not received
    /// an ack for. Notice that we may have already sent this segment; we need to keep these
    /// segments buffered until we receive the ack. Until then, we cannot slide our window.
    l: u64,
    /// The smallest segment number that we have not yet sent. This means we have already sent
    /// every segment < `s`. By definition, `s >= l`. This is our 'cursor.
    s: u64,
    /// The next segment number we expect to be buffered. This means that the last segment in the
    /// buffer must have a segment number of `e - 1`. If the buffer is empty then `e == l`.
    e: u64,
    /// The actual segment buffer
    buffer: Vec<WrappedSegment>,
}

impl SegmentBuffer {
    fn new(l: u64) -> Self {
        Self {
            l,
            s: l,
            e: l,
            buffer: Vec::new(),
        }
    }

    /// Returns an iterator over consumable segments. Segments are consumable if they are within the range
    /// [s, r). This function updates `s`, it is therefore up to the sender to actually use the yielded
    /// segments. Otherwise, they will have to retrieve them manually.
    fn into_iter(&mut self) -> SegmentBufferIter {
        let mut buf = vec![];

        let r = self.l + MAXIMUM_WINDOW_SIZE;
        while self.s - self.l < self.len() as u64 && self.s < r {
            buf.push(self.buffer[(self.s - self.l) as usize].clone());
            self.s += 1;
        }

        SegmentBufferIter::new(buf)
    }

    /// Enqueues segment
    #[inline]
    fn push(&mut self, segment: Segment) {
        self.e = segment.header.packet_number + 1;
        self.buffer.push(WrappedSegment::new(segment));
    }

    /// Try to slide the window left as much as possible
    #[inline]
    fn slide(&mut self) {
        let l = self.l;

        while ((self.l - l) as usize) < self.buffer.len()
            && self.l - l < MAXIMUM_WINDOW_SIZE
            && self.buffer[(self.l - l) as usize].acked
        {
            self.l += 1;
        }

        if l != self.l {
            self.buffer = self.buffer.split_off((self.l - l) as usize);
        }
    }

    /// Returns the left pointer aka the smallest segment number we are waiting for an ack
    #[inline]
    fn waiting_for(&self) -> u64 {
        self.l
    }

    /// Returns the next expected segment number to be buffered eg. the `e` pointer.
    #[inline]
    fn next_expected(&self) -> u64 {
        self.e
    }

    /// Returns the length of the buffer
    #[inline]
    fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Handles an ack and tries to slide the window right if possible
    fn ack(&mut self, segment_number: u64) {
        let ix = segment_number - self.l;

        // Out of bounds
        if segment_number < self.l || ix >= self.buffer.len() as u64 {
            return;
        }

        self.buffer[ix as usize].acked = true;
        self.slide();
    }
}

/// Iterator for consumed segments
#[derive(Debug)]
pub(crate) struct SegmentBufferIter {
    inner: Vec<Segment>,
    ix: usize,
}

impl SegmentBufferIter {
    fn new(inner: Vec<Segment>) -> Self {
        Self { inner, ix: 0 }
    }
}

impl Iterator for SegmentBufferIter {
    type Item = Segment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ix >= self.inner.len() {
            return None;
        }

        self.ix += 1;
        Some(self.inner[self.ix - 1].clone())
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.inner.len()
    }
}

/// Manages stream write buffering.
#[derive(Debug)]
pub(crate) struct WriteStreamManager {
    /// The stream id for this stream manager.
    stream_id: u16,
    /// Queue of our buffered data NOT segments. Once we have built segments out of the data,
    /// we move it into the `segment_buffer`.
    data_buffer: VecDeque<Vec<u8>>,
    /// Stores our buffered segments. Key: segment number, value: segment. Once data is dequeued out
    /// of `data_buffer` and inserted as a segment into `segment_buffer` then the segment is no
    /// longer mutuable. We buffer this in-case the receiving side failed to receive the the data
    /// and we will need to resend the un-acked data.
    segment_buffer: SegmentBuffer,
    src_conn_id: u32,
    dst_conn_id: u32,
}

impl WriteStreamManager {
    pub(crate) fn new(stream_id: u16, left: u64, src_conn_id: u32, dst_conn_id: u32) -> Self {
        Self {
            stream_id,
            data_buffer: VecDeque::new(),
            segment_buffer: SegmentBuffer::new(left),
            src_conn_id,
            dst_conn_id,
        }
    }

    fn create_segment(&self, data_stack: &Vec<Vec<u8>>) -> Segment {
        let squashed = data_stack.concat();
        let segment_num = self.segment_buffer.next_expected();
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            self.src_conn_id,
            self.dst_conn_id,
            PacketType::Stream,
            self.stream_id,
            security_fields,
        );
        header.with_packet_number(segment_num);

        BluefinPacket::builder()
            .header(header)
            .payload(squashed)
            .build()
    }

    /// Attempts to dequeue the data buffered in `data_buffer` and create segments to be buffered in the
    /// `segment_buffer`. This operation is relatively expensive; we greedily try to take as much data as
    /// we can and once we reach a threshold do we finally build a segment. It is possible to override
    /// this greedy behaviour; this will force the implementation to create segments, no matter how small
    /// the data.
    ///
    /// Returns the number of segmements created.
    pub(crate) fn data_to_segments(&mut self) -> usize {
        let mut num_created = 0;
        let mut stack: Vec<Vec<u8>> = vec![];
        let size = usize::min(
            self.data_buffer.len(),
            MAXIMUM_WINDOW_SIZE.try_into().unwrap(),
        );

        // TODO: Re-evaluate this constant
        const SEGMENT_THRESHOLD: usize = 100;

        let mut running_total = 0;

        for _ in 0..size {
            let entry = self.data_buffer.pop_front().unwrap();
            stack.push(entry.clone());

            // Enough data found. Let's pack this into a segment.
            if entry.len() + running_total >= SEGMENT_THRESHOLD {
                let segment = self.create_segment(&stack);
                self.segment_buffer.push(segment);

                stack.clear();
                running_total = 0;
                num_created += 1;
            // Else, we do not have enough data.
            } else {
                running_total += entry.len();
            }
        }

        // Put the remaining data back into the queue, in correct order
        while !stack.is_empty() {
            let entry = stack.pop().unwrap();
            self.data_buffer.push_front(entry);
        }

        num_created
    }

    /// Buffers the write data into a queue. Notice that no segments are created from this invocation.
    #[inline]
    pub(crate) fn enqueue(&mut self, data: Vec<u8>) {
        self.data_buffer.push_back(data);
    }

    /// Consumes the buffered segments are returns the largest allowable iterable of segments ready to
    /// be sent.
    #[inline]
    pub(crate) fn segment_buffer_into_iter(&mut self) -> SegmentBufferIter {
        // We can only send segments within the allowable window. Because the left pointer is the smallest
        // segment number we have not received an ack for, then we can only get segments within the window
        // range [left, MAXIMUM_WINDOW_SIZE).
        self.segment_buffer.into_iter()
    }

    #[inline]
    pub(crate) fn acked(&mut self, acked: u64) {
        self.segment_buffer.ack(acked);
    }
}

impl ReadStreamManager {
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

        let buf = StreamReadBuffer::new(expected);
        let _ = self.stream_map.insert(stream_id, buf);

        Ok(())
    }

    /// Tries to buffer the `segment` into the stream buffer. If the segment number is not within
    /// the buffer's current acceptance window then the data is not buffered and the segment is
    /// dropped.
    pub(crate) fn buffer_to_existing_stream(&mut self, segment: Segment) -> Result<()> {
        let header = segment.header;

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
            segment,
        };

        let entry = self.stream_map.get_mut(&stream_id).unwrap();
        entry.add(buf_data)?;

        Ok(())
    }
}

mod tests {
    use crate::io::stream_manager::MAXIMUM_WINDOW_SIZE;

    use super::WriteStreamManager;

    const STREAM_ID: u16 = 0x1234;
    const LEFT: u64 = 0x12345;
    const SRC_CONN_ID: u32 = 0x13;
    const DST_CONN_ID: u32 = 0x18;

    fn create_write_stream_manager() -> WriteStreamManager {
        WriteStreamManager::new(STREAM_ID, LEFT, SRC_CONN_ID, DST_CONN_ID)
    }

    fn create_payload_of_size(size: usize, val: u8) -> Vec<u8> {
        (0..size).map(|_| val).collect()
    }

    #[test]
    fn write_stream_manager_yields_segments_correctly() {
        let mut stream_manager = create_write_stream_manager();

        stream_manager.enqueue(create_payload_of_size(90, 0x0));
        assert_eq!(stream_manager.data_buffer.len(), 1);
        assert_eq!(stream_manager.data_buffer[0].len(), 90);

        // Payload was only 90 bytes. Only when at least 100 bytes have accumulated
        // do we create segments
        assert_eq!(0, stream_manager.data_to_segments());
        assert_eq!(stream_manager.data_buffer.len(), 1);
        assert_eq!(stream_manager.data_buffer[0].len(), 90);
        assert_eq!(stream_manager.segment_buffer.waiting_for(), LEFT);
        assert_eq!(stream_manager.segment_buffer.next_expected(), LEFT);

        // Only 90 + 9 = 99 bytes buffered, still not enough.
        stream_manager.enqueue(create_payload_of_size(9, 0x1));
        assert_eq!(0, stream_manager.data_to_segments());
        assert_eq!(stream_manager.data_buffer.len(), 2);
        assert_eq!(stream_manager.data_buffer[0].len(), 90);
        assert_eq!(stream_manager.data_buffer[1].len(), 9);
        assert_eq!(stream_manager.segment_buffer.waiting_for(), LEFT);
        assert_eq!(stream_manager.segment_buffer.next_expected(), LEFT);

        // Exactly 101 bytes buffered, one segment created
        stream_manager.enqueue(create_payload_of_size(2, 0x2));
        assert_eq!(1, stream_manager.data_to_segments());
        assert_eq!(stream_manager.data_buffer.len(), 0);
        assert_eq!(stream_manager.segment_buffer.len(), 1);
        // waiting_for should not update just because we were able to create a segment
        assert_eq!(stream_manager.segment_buffer.waiting_for(), LEFT);
        // Next expected updated by 1
        assert_eq!(stream_manager.segment_buffer.next_expected(), LEFT + 1);

        let iter = stream_manager.segment_buffer_into_iter();
        let mut count = 0;
        for seg in iter {
            let header = seg.header;
            let payload = seg.payload;

            assert_eq!(header.source_connection_id, SRC_CONN_ID);
            assert_eq!(header.destination_connection_id, DST_CONN_ID);
            assert_eq!(header.type_specific_payload, STREAM_ID);
            assert_eq!(header.packet_number, LEFT);
            assert_eq!(payload.len(), 101);
            assert_eq!(payload[0], 0x0);
            assert_eq!(payload[89], 0x0);
            assert_eq!(payload[90], 0x1);
            assert_eq!(payload[98], 0x1);
            assert_eq!(payload[99], 0x2);
            assert_eq!(payload[100], 0x2);

            count += 1;
        }
        assert_eq!(count, 1);

        // Handle ack so the window slides by 1
        stream_manager.acked(LEFT);
        assert_eq!(stream_manager.segment_buffer.len(), 0);
        assert_eq!(stream_manager.segment_buffer.waiting_for(), LEFT + 1);

        // Try to get 2 segments; enqueud 310 bytes of data. First segment should get
        // 90 + 90 = 180 bytes and the second segment gets 90 + 40 = 130 bytes.
        stream_manager.enqueue(create_payload_of_size(90, 0x3));
        stream_manager.enqueue(create_payload_of_size(90, 0x4));
        stream_manager.enqueue(create_payload_of_size(90, 0x5));
        stream_manager.enqueue(create_payload_of_size(40, 0x6));

        assert_eq!(2, stream_manager.data_to_segments());
        // Two segments buffered
        assert_eq!(stream_manager.segment_buffer.next_expected(), LEFT + 3);

        count = 0;
        for seg in stream_manager.segment_buffer_into_iter() {
            let header = seg.header;
            let payload = seg.payload;

            assert_eq!(header.source_connection_id, SRC_CONN_ID);
            assert_eq!(header.destination_connection_id, DST_CONN_ID);
            assert_eq!(header.type_specific_payload, STREAM_ID);
            assert_eq!(header.packet_number, LEFT + 1 + count);

            if count == 0 {
                assert_eq!(payload.len(), 180);
            } else {
                assert_eq!(payload.len(), 130);
            }

            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn write_stream_manager_does_not_yield_after_r() {
        let mut manager = create_write_stream_manager();
        // we can only yield up to 100 segments from l. Beyond that we yield nothing
        // until we slide l.
        // Enqueue 100 * 12 = 1200 bytes.
        for _ in 0..=101 {
            let payload = create_payload_of_size(100, 0x0);
            manager.enqueue(payload);
        }

        // Only 100 segments created
        assert_eq!(100, manager.data_to_segments());

        // Last segments created
        assert_eq!(2, manager.data_to_segments());

        // Nothing left
        assert_eq!(0, manager.data_to_segments());

        // Check that segments buffer is in correct state
        assert_eq!(LEFT + 102, manager.segment_buffer.e);
        assert_eq!(LEFT, manager.segment_buffer.l);
        assert_eq!(LEFT, manager.segment_buffer.s);

        // Assert that the `s` pointer behaves as expected
        let first_iter = manager.segment_buffer_into_iter();
        let mut first_count = 0;
        for seg in first_iter {
            assert_eq!(seg.header.packet_number, LEFT + first_count);
            first_count += 1;
        }
        // Only 100 segments yielded despite their being 102 segments in buffer
        assert_eq!(100, first_count);
        // We yielded everything in [LEFT, LEFT + 99]. We have not yielded LEFT + 100 and
        // LEFT + 101 because we didn't slide our window.
        assert_eq!(LEFT + 100, manager.segment_buffer.s);
        assert_eq!(LEFT + 102, manager.segment_buffer.e);
        assert_eq!(LEFT, manager.segment_buffer.l);

        // Did not yield anything because `s` is at `r`.
        assert_eq!(0, manager.segment_buffer_into_iter().count());
        assert_eq!(102, manager.segment_buffer.len());

        // Slide window by 1
        manager.acked(LEFT);
        assert_eq!(101, manager.segment_buffer.len());
        assert_eq!(LEFT + 1, manager.segment_buffer.l);

        // Only yield one segment since we only slid by one
        assert_eq!(1, manager.segment_buffer_into_iter().count());
        assert_eq!(LEFT + 101, manager.segment_buffer.s);

        // Slide window by another
        manager.acked(LEFT + 1);
        assert_eq!(100, manager.segment_buffer.len());
        assert_eq!(LEFT + 2, manager.segment_buffer.l);
        assert_eq!(1, manager.segment_buffer_into_iter().count());
        assert_eq!(LEFT + 102, manager.segment_buffer.s);
    }

    #[test]
    fn write_stream_manager_handles_acks_correctly() {
        let mut manager = create_write_stream_manager();
        // Enqueue 100 * 105 = 10,500 bytes or 105 segments
        for _ in 0..105 {
            manager.enqueue(create_payload_of_size(100, 0x0));
        }

        // Maximum number of segments created
        assert_eq!(MAXIMUM_WINDOW_SIZE as usize, manager.data_to_segments());
        // Five segments remaining
        assert_eq!(5 as usize, manager.data_to_segments());

        // Max number of segments yielded
        assert_eq!(
            MAXIMUM_WINDOW_SIZE as usize,
            manager.segment_buffer_into_iter().count()
        );

        // Can't yield anymore segments as we have hit the window
        assert_eq!(0, manager.segment_buffer_into_iter().count());

        // LEFT LEFT+1 LEFT+2 LEFT+3 ...
        // Ack LEFT + 3 but we still need to ack LEFT, no sliding
        manager.acked(LEFT + 3);
        assert_eq!(0, manager.segment_buffer_into_iter().count());

        manager.acked(LEFT + 1);
        assert_eq!(0, manager.segment_buffer_into_iter().count());

        manager.acked(LEFT + 2);
        assert_eq!(0, manager.segment_buffer_into_iter().count());

        // Finally, missing piece acked. We slide the window by four.
        manager.acked(LEFT);
        assert_eq!(4, manager.segment_buffer_into_iter().count());

        // Get the last segment
        manager.acked(LEFT + 4);
        assert_eq!(1, manager.segment_buffer_into_iter().count());

        // Nothing left
        manager.acked(LEFT + 5);
        assert_eq!(0, manager.segment_buffer_into_iter().count());
    }
}
