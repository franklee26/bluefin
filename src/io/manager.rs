use crate::core::error::BluefinError;
use crate::core::packet::BluefinPacket;
use std::result;
use std::{collections::HashMap, task::Waker};

/// Bluefin Result yields a BluefinError
pub type Result<T> = result::Result<T, BluefinError>;

/// Represents the buffered bytes for a connection.
#[derive(Debug)]
pub(crate) struct ConnectionBuffer {
    packet: Option<BluefinPacket>,
}

impl ConnectionBuffer {
    pub(crate) fn new() -> Self {
        Self { packet: None }
    }

    /// Adds `packet` to the connection's buffer. If the buffer is full then
    /// an error is returned.
    pub(crate) fn add_to_buffer(&mut self, packet: BluefinPacket) -> Result<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(packet);
        Ok(())
    }

    /// Consumes the buffer. If there is nothing in the buffer then an error
    /// is returned. Otherwise the packet is yielded and the packet is dropped
    /// from the buffer.
    pub(crate) fn consume(&mut self) -> Result<BluefinPacket> {
        if self.packet.is_none() {
            return Err(BluefinError::BufferEmptyError);
        }

        Ok(self.packet.take().unwrap())
    }
}

#[derive(Debug)]
pub(crate) struct ConnectionManager {
    /// A connection is identified by the key `<other_id>_<mine_id>`. For example if a client
    /// with id `abc` connected to the current host with conn id `def` then the host would
    /// look for `abc_def`. In the case that a client is requesting a new connection then
    /// then the host would expect the `dst_id` to be `0x0` (see handshake protocol).
    connection_map: HashMap<String, ConnectionBuffer>,

    /// Maps a given connection id to it's associated waker.
    waker_map: HashMap<String, Waker>,

    /// A vector of wakers awaiting a new connection
    pending_new_connection_wakers: Vec<Waker>,
}

impl ConnectionManager {
    pub(crate) fn new() -> Self {
        Self {
            connection_map: HashMap::new(),
            waker_map: HashMap::new(),
            pending_new_connection_wakers: vec![],
        }
    }

    /// Inserts a new buffer into the manager map. If the connection already exists then
    /// this function returns an error. Otherwise, we add the new entry into the map and
    /// signal the waker to wake such that any read processes can continue.
    pub(crate) fn add_new_connection(
        &mut self,
        dst_id: u32,
        src_id: u32,
        waker: Waker,
    ) -> Result<()> {
        let key = format!("{}_{}", dst_id, src_id);

        // This connection already exists...
        if self.conn_exists(&key) {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        // Generate new buffer and add
        let conn_buf = ConnectionBuffer::new();
        self.connection_map.insert(key, conn_buf);

        // Signal to executor there was a change in the connection map
        waker.wake();
        Ok(())
    }

    /// Pushes a new connection waker. These wakers are for requests for `Accept`
    /// futures and are different from normal read futures
    pub(crate) fn add_new_connection_waker(&mut self, waker: Waker) {
        self.pending_new_connection_wakers.push(waker);
    }

    /// Pops the top of stack waker and wakes. The order here doesn't matter since
    /// we are just requesting a new connection and there is no state yet.
    pub(crate) fn wake_new_connection(&mut self) -> Result<()> {
        if self.pending_new_connection_wakers.is_empty() {
            return Err(BluefinError::NoSuchWakerError);
        }

        let waker = self.pending_new_connection_wakers.pop().unwrap();
        waker.wake();

        Ok(())
    }

    /// Adds a packet to an existing connection. If the connection does not exist
    /// then this function returns an error.
    pub(crate) fn add_buffer_to_connection(
        &mut self,
        key: &str,
        packet: BluefinPacket,
    ) -> Result<()> {
        if !self.conn_exists(&key) {
            return Err(BluefinError::NoSuchConnectionError);
        }

        self.connection_map
            .get_mut(key)
            .unwrap()
            .add_to_buffer(packet)?;

        // Signal to executor there was a change in the connection map
        self.wake(key)?;
        Ok(())
    }

    pub(crate) fn add_waker(&mut self, key: &str, waker: Waker) -> Result<()> {
        if self.waker_map.contains_key(key) {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        self.waker_map.insert(key.to_string(), waker);

        Ok(())
    }

    /// Wakes the associated connection's waker and removes the waker from the map
    pub(crate) fn wake(&mut self, key: &str) -> Result<()> {
        if !self.waker_map.contains_key(key) {
            return Err(BluefinError::NoSuchConnectionError);
        }

        // Wake the waker
        self.waker_map.get(key).unwrap().wake_by_ref();

        // Remove from map
        self.waker_map.remove_entry(key);

        Ok(())
    }

    /// Checks whether a given connection is already opened. `dst_id` refers to the
    /// incoming packet's destination id (aka. the current host's conn id) and
    /// `src_id` refers to what the other host's id is.
    pub(crate) fn conn_exists(&self, key: &str) -> bool {
        self.connection_map.contains_key(key)
    }
}
