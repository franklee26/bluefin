use std::{cmp::min, net::SocketAddr};

use crate::{
    core::{error::BluefinError, packet::BluefinPacket},
    utils::common::BluefinResult,
};

const MAX_BUFFER_SIZE: usize = 2000;
pub(crate) const MAX_BUFFER_CONSUME: usize = 1000;

/// A `ConnectionBuffer` is a future of buffered packets. For now, the buffer only
/// holds at most 2000 bytes.
#[derive(Clone)]
pub(crate) struct ConnectionBuffer {
    bytes: Vec<u8>,
    addr: Option<SocketAddr>,
}

impl ConnectionBuffer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            addr: None,
        }
    }

    #[inline]
    pub(crate) fn have_bytes(&self) -> bool {
        !self.bytes.is_empty()
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.bytes.len() >= MAX_BUFFER_SIZE
    }

    #[inline]
    pub(crate) fn buffer_in_addr(&mut self, addr: SocketAddr) -> BluefinResult<()> {
        if let Some(_) = self.addr {
            return Err(BluefinError::Unexpected(
                "Address already exists".to_string(),
            ));
        }

        self.addr = Some(addr);
        Ok(())
    }

    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: &mut BluefinPacket) -> BluefinResult<()> {
        if self.is_full() {
            return Err(BluefinError::BufferFullError);
        }

        self.bytes.append(&mut packet.payload);

        Ok(())
    }

    #[inline]
    pub(crate) fn consume(&mut self) -> BluefinResult<(Vec<u8>, SocketAddr)> {
        if !self.have_bytes() {
            return Err(BluefinError::BufferEmptyError);
        }

        if self.addr.is_none() {
            eprintln!("Consume: empty addr");
            /*
            return Err(BluefinError::Unexpected(
                "Address not found, could not consume".to_string(),
            ));
            */
        }

        let num_bytes_to_consume = min(MAX_BUFFER_CONSUME, self.bytes.len());

        let ans = (
            self.bytes.drain(..num_bytes_to_consume).collect(),
            self.addr.unwrap(),
        );

        return Ok(ans);
    }
}
