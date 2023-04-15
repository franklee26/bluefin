use crate::core::error::BluefinError;
use crate::core::packet::Packet;
use crate::utils::common::SyncConnBufferRef;
use std::collections::VecDeque;
use std::result;
use std::sync::Arc;
use std::{collections::HashMap, task::Waker};

/// Bluefin Result yields a BluefinError
pub type Result<T> = result::Result<T, BluefinError>;

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

    /// Adds just the `packet` to the connection's buffer. If the buffer is
    /// full then an error is returned.
    pub(crate) fn add_to_buffer(&mut self, packet: Packet) -> Result<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(packet);
        Ok(())
    }

    /// Consumes the buffer. If there is nothing in the buffer then nothing
    /// is returned. Otherwise the packet is yielded and the packet + waker
    /// are dropped from the buffer.
    pub(crate) fn consume(&mut self) -> Option<Packet> {
        self.waker = None;

        if self.packet.is_none() {
            return None;
        }

        Some(self.packet.take().unwrap())
    }
}

/// Buffer at most 10 unhandled requests. Otherwise, we might get overburdended with
/// these requests.
const MAX_UNHANDLED_NEW_CONNECTION_REQ: usize = 10;

#[derive(Debug)]
pub(crate) struct ConnectionManager {
    /// Maps a connection to its connection buffer. At any moment in time, there can be at most
    /// two actors trying to access this buffer: the connection itself or this manager. Because
    /// accessing the buffer is done in the `BufferedRead` poll's implementation, this mutex
    /// must be blocking.
    buffer_map: HashMap<String, SyncConnBufferRef>,

    /// Serves the exact same purpose as `buffer_map` except it is intended for new `Accept` requests
    /// only. This makes it easy for us to find packets that should be directed to `BufferedReads`
    /// awaiting the client-hello.
    accept_buffer_map: HashMap<String, SyncConnBufferRef>,

    /// Buffers in client hello packets that couldn't be buffered in `accept_buffer_map` since there
    /// was no outstanding `Accept` requests.
    unhandled_client_hellos: VecDeque<Packet>,
}

impl ConnectionManager {
    pub(crate) fn new() -> Self {
        Self {
            buffer_map: HashMap::new(),
            accept_buffer_map: HashMap::new(),
            unhandled_client_hellos: VecDeque::new(),
        }
    }

    /// Attempts to buffer the first packet in `unhandled_client_hellos` into the first `Accept`
    /// buffer we find
    pub(crate) fn try_to_wake_buffered_accept(&mut self) -> Result<()> {
        // Can't do anything if either of these buffers are empty
        if self.accept_buffer_map.is_empty() || self.unhandled_client_hellos.is_empty() {
            return Err(BluefinError::BufferEmptyError);
        }

        let packet = self.unhandled_client_hellos.pop_front().unwrap();
        let cloned = self.accept_buffer_map.clone();
        let (key, buf) = cloned.iter().next().unwrap();

        let mut guard = buf.lock().unwrap();
        guard.add_to_buffer(packet)?;

        if let Some(waker) = &guard.waker {
            waker.wake_by_ref();
        }

        let _ = self.accept_buffer_map.remove(key);

        Ok(())
    }

    pub(crate) fn buffer_unhandled_client_hello(&mut self, packet: Packet) -> Result<()> {
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
    pub(crate) fn register_new_accept_connection(&mut self, key: &str) -> SyncConnBufferRef {
        let buf = Arc::new(std::sync::Mutex::new(ConnectionBuffer::new()));

        let _ = self
            .accept_buffer_map
            .insert(key.to_string(), Arc::clone(&buf));

        buf
    }

    /// Returns whether or not a given key has been registered in the manager
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
    /// `accept_buffer_map`. If such an entry exists then we try to signal it's waker. Else, this
    /// function errors.
    pub(crate) fn buffer_client_hello(&mut self, packet: Packet) -> Result<()> {
        // Can't buffer this!
        if self.accept_buffer_map.is_empty() {
            return self.buffer_unhandled_client_hello(packet);
        }

        for (key, val) in self.accept_buffer_map.clone().iter() {
            let mut guard = val.lock().unwrap();

            // buffer in packet
            if let Ok(_) = guard.add_to_buffer(packet.clone()) {
                // wake waker
                if let Some(waker) = &guard.waker {
                    waker.wake_by_ref();
                }

                // delete this entry
                self.accept_buffer_map.remove(key);

                return Ok(());
            }
        }

        // TODO: return better error
        Err(BluefinError::BufferEmptyError)
    }
}
