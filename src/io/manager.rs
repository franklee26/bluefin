use crate::core::error::BluefinError;
use crate::core::packet::Packet;
use crate::set_waker;
use crate::utils::common::SyncConnBufferRef;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::task::Waker;

use super::Buffer;
use super::Result;

/// Buffer at most 10 unhandled requests. Otherwise, we might get overburdended with
/// these requests.
const MAX_UNHANDLED_NEW_CONNECTION_REQ: usize = 10;

/// Represents the buffered bytes for a connection.
#[derive(Clone, Debug)]
pub(crate) struct ConnectionBuffer {
    pub(crate) packet: Option<Packet>,
    pub(crate) waker: Option<Waker>,
}

impl ConnectionBuffer {
    pub(crate) fn new() -> Self {
        Self {
            packet: None,
            waker: None,
        }
    }
}

impl Buffer for ConnectionBuffer {
    type BufferData = Packet;
    type ConsumedData = Self::BufferData;

    /// Adds just the `packet` to the connection's buffer. If the buffer is
    /// full then an error is returned.
    #[inline]
    fn add(&mut self, data: Self::BufferData) -> Result<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(data);
        Ok(())
    }

    /// Consumes the buffer. If there is nothing in the buffer then nothing
    /// is returned. Otherwise the packet is yielded and the packet + waker
    /// are dropped from the buffer.
    #[inline]
    fn consume(&mut self) -> Option<Self::ConsumedData> {
        self.waker = None;

        self.packet.take()
    }

    set_waker!();
}

#[derive(Debug)]
pub(crate) struct ConnectionManager {
    /// Maps a connection to its connection buffer. At any moment in time, there can be at most
    /// two actors trying to access this buffer: the connection itself or this manager. Because
    /// accessing the buffer is done in the `BufferedRead` poll's implementation, this mutex
    /// must be blocking.
    buffer_map: HashMap<String, SyncConnBufferRef>,

    /// Serves the exact same purpose as `buffer_map` except it is intended for new `Accept` requests
    /// only. This makes it easy for us to find packets that should be directed to `BufferedReads`
    /// awaiting the client-hello. Notice that the order of this is important; we want to handle
    /// accept's in an FIFO mannger.
    accept_buffer_queue: VecDeque<SyncConnBufferRef>,

    /// Buffers in client hello packets that couldn't be buffered in `accept_buffer_map` since there
    /// was no outstanding `Accept` requests.
    unhandled_client_hellos: VecDeque<Packet>,
}

impl ConnectionManager {
    pub(crate) fn new() -> Self {
        Self {
            buffer_map: HashMap::new(),
            accept_buffer_queue: VecDeque::new(),
            unhandled_client_hellos: VecDeque::new(),
        }
    }

    /// Attempts to buffer the first packet in `unhandled_client_hellos` into the first `Accept`
    /// buffer we find
    pub(crate) fn try_to_wake_buffered_accept(&mut self) -> Result<()> {
        // Can't do anything if either of these buffers are empty
        if self.accept_buffer_queue.is_empty() || self.unhandled_client_hellos.is_empty() {
            return Err(BluefinError::BufferEmptyError);
        }

        let packet = self.unhandled_client_hellos.pop_front().unwrap();
        let accept_buf = self.accept_buffer_queue.pop_front().unwrap();

        let mut guard = accept_buf.lock().unwrap();
        guard.add(packet)?;

        if let Some(waker) = &guard.waker {
            waker.wake_by_ref();
        }

        Ok(())
    }

    /// Buffers in an unhandled client hello `packet`. This happens when we receive a client-hello
    /// but we can't find any pending `Accept` requests in `accept_buffer_map`. We store this for
    /// later use incase another `Accept` request comes and can potentially pick up this unhandled
    /// packet. In order to avoid burdening memory, we only buffer a limited number of these packets.
    /// TODO: Add TTL for these packets
    #[inline]
    fn buffer_unhandled_client_hello(&mut self, packet: Packet) -> Result<()> {
        if self.unhandled_client_hellos.len() >= MAX_UNHANDLED_NEW_CONNECTION_REQ {
            return Err(BluefinError::BufferFullError);
        }

        self.unhandled_client_hellos.push_back(packet);

        Ok(())
    }

    /// Registers a new connection
    pub(crate) fn register_new_connection(
        &mut self,
        key: &str,
        buffer: Arc<std::sync::Mutex<ConnectionBuffer>>,
    ) {
        let _ = self.buffer_map.insert(key.to_string(), buffer);
    }

    /// Registers a new accept buffer, creates its shared buffer and returns an Arc<> reference to it
    pub(crate) fn register_new_accept_connection(&mut self) -> SyncConnBufferRef {
        let buf = Arc::new(std::sync::Mutex::new(ConnectionBuffer::new()));

        self.accept_buffer_queue.push_back(Arc::clone(&buf));

        buf
    }

    /// Returns whether or not a given key has been registered in the manager
    #[inline]
    pub(crate) fn buffer_exists(&self, key: &str) -> bool {
        self.buffer_map.contains_key(key)
    }

    /// Adds a packet to an existing connection buffer. If the connection does not exist then this errors. Notice
    /// that this function is blocking; it will block until the current thread acquires the lock for this buffer.
    pub(crate) fn buffer_packet_to_existing_connection(
        &mut self,
        key: &str,
        packet: Packet,
    ) -> Result<()> {
        if !self.buffer_exists(key) {
            return Err(BluefinError::NoSuchConnectionError);
        }

        {
            let mut guard = self.buffer_map.get(key).unwrap().lock().unwrap();
            guard.packet = Some(packet);

            // Signal the waker if it exists
            if let Some(waker) = &guard.waker {
                waker.wake_by_ref();
                // Consumed.
                guard.waker = None;
            }
        }

        // We need to delete this entry if it's just a client first-read
        if key.starts_with("0_") {
            self.buffer_map.remove(key);
        }

        Ok(())
    }

    /// Takes in a valid client-hello `packet` and tries to buffer it into the oldest entry in the
    /// `accept_buffer_queue`. If such an entry exists then we try to signal it's waker. Else, this
    /// function errors.
    pub(crate) fn buffer_client_hello(&mut self, packet: Packet) -> Result<()> {
        // Can't buffer this!
        if self.accept_buffer_queue.is_empty() {
            return self.buffer_unhandled_client_hello(packet);
        }

        let mut winner_packet = packet.clone();

        // If there are already unhandled client-hello packets, then they should go first
        if !self.unhandled_client_hellos.is_empty() {
            winner_packet = self.unhandled_client_hellos.pop_front().unwrap();

            // Buffer in this packet as unhandled
            self.unhandled_client_hellos.push_back(packet);
        }

        let first_buf = self.accept_buffer_queue.pop_front().unwrap();
        let mut guard = first_buf.lock().unwrap();

        // buffer in packet
        if let Ok(_) = guard.add(winner_packet) {
            // wake waker
            if let Some(waker) = &guard.waker {
                waker.wake_by_ref();
            }

            return Ok(());
        }

        // TODO: return better error
        Err(BluefinError::BufferEmptyError)
    }
}
