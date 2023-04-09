use crate::core::error::BluefinError;
use crate::core::packet::Packet;
use std::result;
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

    /// Create a new buffer with just a waker.
    pub(crate) fn new_with_waker(waker: Waker) -> Self {
        Self {
            packet: None,
            waker: Some(waker),
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

    /// Adds `packet` and its associated waker to the connection's buffer.
    /// If the buffer is full then an error is returned.
    pub(crate) fn add_to_buffer_with_waker(&mut self, packet: Packet, waker: Waker) -> Result<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(packet);
        self.waker = Some(waker);
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

#[derive(Debug)]
pub(crate) struct ConnectionManager {
    /// A connection is identified by the key `<other_id>_<mine_id>`. For example if a client
    /// with key `abc` connected to the current host with conn id `def` then the host would
    /// look for `abc_def`. In the case that a client is requesting a new connection then
    /// then the host would expect the `dst_id` to be `0x0` (see handshake protocol). This means
    /// the key for a new connection request from client `abc` would be `abc_0`.
    connection_map: HashMap<String, ConnectionBuffer>,

    /// A map tracking new connection `Accept`'s. Because a new connection does not have a defined
    /// id, we use the `Accept`'s unique id to track each waker. We need a separate map because we
    /// need to quickly identify if a buffer applies to a normal socket read or if the buffer was
    /// buffering a new connection request.
    new_connection_req_map: HashMap<String, ConnectionBuffer>,
}

impl ConnectionManager {
    pub(crate) fn new() -> Self {
        Self {
            connection_map: HashMap::new(),
            new_connection_req_map: HashMap::new(),
        }
    }

    /// Register an `Accept`'s new connection request. If we have already registered a buffer
    /// for this `Accept` request then this function errors.
    pub(crate) fn register_new_connection_request(
        &mut self,
        accept_id: String,
        waker: Waker,
    ) -> Result<()> {
        if self.new_connection_req_map.contains_key(&accept_id) {
            return Err(BluefinError::BufferFullError);
        }

        let conn_buf = ConnectionBuffer::new_with_waker(waker);

        self.new_connection_req_map.insert(accept_id, conn_buf);
        Ok(())
    }

    /// Registers a new connection. The key here may take the usual `<other>_<mine>` id form
    /// OR it can just take the form `0_<mine>` when the client is initiating the handshake
    /// and is waiting for the pack leader's response.
    pub(crate) fn register_new_connection(&mut self, key: String) {
        let conn_buf = ConnectionBuffer::new();
        self.connection_map.insert(key, conn_buf);
    }

    /// Adds a packet to an existing connection. If the connection does not exist
    /// then this function returns an error.
    pub(crate) fn buffer_to_existing_connection(
        &mut self,
        key: &str,
        packet: Packet,
    ) -> Result<()> {
        if !self.conn_exists(&key) {
            return Err(BluefinError::NoSuchConnectionError);
        }

        let conn_buf = self.connection_map.get_mut(key).unwrap();

        // Add the packet to buffer
        conn_buf.add_to_buffer(packet)?;

        // wake up future
        conn_buf.waker.as_ref().unwrap().wake_by_ref();

        Ok(())
    }

    /// Adds a packet to any pending new connection request buffers. If there are no new connection
    /// requests then we throw away the packet and nothing happens. Otherwise, we pick the first
    /// pending request (FIFO), buffer the packet and signal the waker.
    pub(crate) fn buffer_to_new_connection_request(&mut self, packet: Packet) {
        for (_, conn_buf) in &mut self.new_connection_req_map {
            // I need a waker!
            if conn_buf.waker.is_none() {
                continue;
            }

            (*conn_buf).packet = Some(packet);
            conn_buf.waker.as_ref().unwrap().wake_by_ref();

            return;
        }
    }

    /// Removes a new connection request with accept_id `key`. It's important to remove these requests
    /// from the manager such that we don't re-awake an already woken new connection request poll.
    pub(crate) fn remove_new_conn_req(&mut self, key: String) -> Result<()> {
        if !self.new_connection_req_map.contains_key(&key) {
            // TODO: fix error
            return Err(BluefinError::NoSuchConnectionError);
        }

        let _ = self.new_connection_req_map.remove(&key);

        Ok(())
    }

    /// Checks whether a given connection is already opened. `dst_id` refers to the
    /// incoming packet's destination id (aka. the current host's conn id) and
    /// `src_id` refers to what the other host's id is.
    pub(crate) fn conn_exists(&self, key: &str) -> bool {
        self.connection_map.contains_key(key)
    }

    /// Fetches the connection buffer for a given connection `key` and resets the buffer content
    /// and drops the waker (if they exist)
    pub(crate) fn consume_conn_buf(&mut self, key: String) -> Option<Packet> {
        if let Some(conn_buff) = self.connection_map.get_mut(&key) {
            return conn_buff.consume();
        }

        None
    }

    pub(crate) fn add_waker_to_existing_conn(&mut self, key: String, waker: Waker) {
        if !self.conn_exists(&key) {
            return;
        }
        let conn_buf = self.connection_map.get_mut(&key).unwrap();

        // No need to re-register
        if conn_buf.waker.is_some() {
            return;
        }

        // Register waker
        (*conn_buf).waker = Some(waker);
    }

    /// Checks whether there is already a registered buffer for a new connection request
    /// with key `accept_id`. If there is no such entry then we return none.
    pub(crate) fn search_new_conn_req_buffer(
        &self,
        accept_id: String,
    ) -> Option<&ConnectionBuffer> {
        self.new_connection_req_map.get(&accept_id)
    }
}
