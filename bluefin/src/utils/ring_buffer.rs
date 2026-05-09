/// A fixed-capacity ring buffer optimized for hot-path performance.
/// Zero allocations after initialization.
/// 
/// Typical usage: queue data packets or ack data in writer tasks.
#[derive(Debug)]
pub(crate) struct RingBuffer<T> {
    buffer: Vec<T>,
    capacity: usize,
    head: usize,  // Read position
    tail: usize,  // Write position
    len: usize,   // Current number of elements
}

impl<T> RingBuffer<T> {
    /// Creates a new ring buffer with fixed capacity.
    /// Pre-allocates all memory upfront.
    #[inline]
    pub(crate) fn new(capacity: usize) -> Self
    where
        T: Default + Clone,
    {
        Self {
            buffer: vec![T::default(); capacity],
            capacity,
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    /// Returns the number of elements currently in the buffer.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the buffer is empty.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns true if the buffer is full.
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.len == self.capacity
    }

    /// Pushes an element to the back of the buffer.
    /// Returns None if successful, or Some(value) if buffer is full.
    #[inline]
    pub(crate) fn push_back(&mut self, value: T) -> Option<T> {
        if self.is_full() {
            return Some(value);
        }

        self.buffer[self.tail] = value;
        self.tail = (self.tail + 1) % self.capacity;
        self.len += 1;
        None
    }

    /// Pushes an element to the front of the buffer.
    /// Returns None if successful, or Some(value) if buffer is full.
    #[inline]
    pub(crate) fn push_front(&mut self, value: T) -> Option<T> {
        if self.is_full() {
            return Some(value);
        }

        self.head = if self.head == 0 {
            self.capacity - 1
        } else {
            self.head - 1
        };
        self.buffer[self.head] = value;
        self.len += 1;
        None
    }

    /// Pops an element from the front of the buffer.
    /// Returns None if buffer is empty.
    #[inline]
    pub(crate) fn pop_front(&mut self) -> Option<T>
    where
        T: Default,
    {
        if self.is_empty() {
            return None;
        }

        let value = std::mem::take(&mut self.buffer[self.head]);
        self.head = (self.head + 1) % self.capacity;
        self.len -= 1;
        Some(value)
    }

    /// Peeks at the front element without removing it.
    #[inline]
    pub(crate) fn front(&self) -> Option<&T> {
        if self.is_empty() {
            return None;
        }
        Some(&self.buffer[self.head])
    }

    /// Clears all elements from the buffer.
    #[inline]
    pub(crate) fn clear(&mut self)
    where
        T: Default,
    {
        while !self.is_empty() {
            self.pop_front();
        }
    }
}

impl<T: Default + Clone> Default for RingBuffer<T> {
    fn default() -> Self {
        Self::new(128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let mut rb = RingBuffer::new(3);
        assert!(rb.is_empty());
        assert_eq!(rb.len(), 0);

        assert!(rb.push_back(1).is_none());
        assert!(rb.push_back(2).is_none());
        assert!(rb.push_back(3).is_none());
        assert!(rb.is_full());

        // Should reject when full
        assert_eq!(rb.push_back(4), Some(4));

        assert_eq!(rb.pop_front(), Some(1));
        assert_eq!(rb.pop_front(), Some(2));
        assert_eq!(rb.pop_front(), Some(3));
        assert!(rb.is_empty());
        assert_eq!(rb.pop_front(), None);
    }

    #[test]
    fn test_ring_buffer_wraparound() {
        let mut rb = RingBuffer::new(3);
        
        rb.push_back(1);
        rb.push_back(2);
        assert_eq!(rb.pop_front(), Some(1));
        
        rb.push_back(3);
        rb.push_back(4);
        
        assert_eq!(rb.pop_front(), Some(2));
        assert_eq!(rb.pop_front(), Some(3));
        assert_eq!(rb.pop_front(), Some(4));
        assert!(rb.is_empty());
    }
}
