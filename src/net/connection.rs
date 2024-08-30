use std::{
    cmp::min,
    collections::HashMap,
    future::Future,
    io::Write,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};

use tokio::net::UdpSocket;

use crate::{
    core::{
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        Serialisable,
    },
    utils::common::BluefinResult,
    worker::reader::RxChannel,
};

const MAX_BUFFER_SIZE: usize = 2000;
pub(crate) const MAX_BUFFER_CONSUME: usize = 1000;

/// A `ConnectionBuffer` is a future of buffered packets. For now, the buffer only
/// holds at most 2000 bytes.
#[derive(Clone)]
pub(crate) struct ConnectionBuffer {
    bytes: Vec<u8>,
    addr: Option<SocketAddr>,
    waker: Option<Waker>,
    packet: Option<BluefinPacket>,
    dst_conn_id: u32,
}

#[derive(Clone)]
pub(crate) struct HandshakeConnectionBuffer {
    conn_buff: Arc<Mutex<ConnectionBuffer>>,
}

impl HandshakeConnectionBuffer {
    pub(crate) fn new(conn_buff: Arc<Mutex<ConnectionBuffer>>) -> Self {
        Self { conn_buff }
    }

    pub(crate) async fn read(&self) -> (BluefinPacket, SocketAddr) {
        self.clone().await
    }
}

impl Future for HandshakeConnectionBuffer {
    type Output = (BluefinPacket, SocketAddr);

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.conn_buff.lock().unwrap();
        if let (Some(packet), Some(addr)) = (guard.packet.take(), guard.addr) {
            return Poll::Ready((packet, addr));
        }
        guard.set_waker(cx.waker().clone());
        drop(guard);

        Poll::Pending
    }
}

impl ConnectionBuffer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            addr: None,
            waker: None,
            packet: None,
            dst_conn_id: 0,
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
    pub(crate) fn set_dst_conn_id(&mut self, dst_conn_id: u32) {
        self.dst_conn_id = dst_conn_id;
    }

    #[inline]
    pub(crate) fn get_dst_conn_id(&self) -> u32 {
        self.dst_conn_id
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
    pub(crate) fn buffer_in_bytes(&mut self, packet: &mut BluefinPacket) -> BluefinResult<()> {
        if self.is_full() {
            return Err(BluefinError::BufferFullError);
        }

        self.bytes.append(&mut packet.payload);

        Ok(())
    }

    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: BluefinPacket) -> BluefinResult<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(packet);

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

    #[inline]
    pub(crate) fn get_waker(&self) -> Option<&Waker> {
        self.waker.as_ref()
    }

    #[inline]
    pub(crate) fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker);
    }
}

pub(crate) struct ConnectionManager {
    /// Key: {src_conn_id}_{dst_conn_id}
    /// Value: The connectino buffer
    map: HashMap<String, Arc<Mutex<ConnectionBuffer>>>,
}

impl ConnectionManager {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        key: &str,
        element: Arc<Mutex<ConnectionBuffer>>,
    ) -> BluefinResult<()> {
        if self.map.contains_key(key) {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        self.map.insert(key.to_string(), element);

        Ok(())
    }

    pub(crate) fn get(&self, key: &str) -> Option<Arc<Mutex<ConnectionBuffer>>> {
        self.map.get(key).cloned()
    }

    pub(crate) fn remove(&mut self, key: &str) -> BluefinResult<()> {
        Ok(())
    }
}

pub struct BluefinConnection {
    pub src_conn_id: u32,
    pub dst_conn_id: u32,
    socket: Arc<UdpSocket>,
    conn_buffer: Arc<Mutex<ConnectionBuffer>>,
    rx: RxChannel,
}

impl BluefinConnection {
    pub(crate) fn new(
        src_conn_id: u32,
        dst_conn_id: u32,
        conn_buffer: Arc<Mutex<ConnectionBuffer>>,
        socket: Arc<UdpSocket>,
    ) -> Self {
        let rx = RxChannel::new(src_conn_id, Arc::clone(&conn_buffer));
        Self {
            src_conn_id,
            dst_conn_id,
            rx,
            socket,
            conn_buffer: Arc::clone(&conn_buffer),
        }
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> BluefinResult<usize> {
        let (bytes, addr) = self.rx.read().await;
        let size = buf.as_mut().write(&bytes)?;
        return Ok(size);
    }

    pub async fn send(&self, buf: &[u8]) -> BluefinResult<usize> {
        // create bluefin packet and send
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let header = BluefinHeader::new(
            self.src_conn_id,
            self.dst_conn_id,
            PacketType::UnencryptedData,
            0x0,
            security_fields,
        );
        let packet = BluefinPacket::builder()
            .header(header)
            .payload(buf.to_vec())
            .build();
        let bytes = self.socket.send(&packet.serialise()).await?;

        Ok(bytes)
    }
}
