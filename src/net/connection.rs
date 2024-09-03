use std::{
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
        context::BluefinHost,
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        Serialisable,
    },
    utils::common::BluefinResult,
    worker::reader::RxChannel,
};

use super::ordered_bytes::OrderedBytes;

pub const MAX_BUFFER_SIZE: usize = 2000;
pub const MAX_BUFFER_CONSUME: usize = 1000;

/// HandshakeConnectionBuffer is a wrapper around the shared ConnectionBuffer. We need this
/// wrapper as it serves as a special future for handling handshake scenarios.
/// [HandshakeConnectionBuffer::read] this future yields a single bluefin packet and socket
/// address information. The bluefin packet is guaranteed to be an UnencryptedClientHello,
/// UnencryptedServerHello or Ack from the client (signalling the completion of the handshake).
#[derive(Clone)]
pub(crate) struct HandshakeConnectionBuffer {
    conn_buff: Arc<Mutex<ConnectionBuffer>>,
}

impl HandshakeConnectionBuffer {
    pub(crate) fn new(conn_buff: Arc<Mutex<ConnectionBuffer>>) -> Self {
        Self { conn_buff }
    }

    #[inline]
    /// Awaits the future for a handshake-related packet stored in the [HandshakeConnectionBuffer::conn_buff].
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

/// ConnectionBuffer as the name suggests is a buffer allocated per connection. This buffer
/// is shared between reader jobs and the actual owning connection. For usual connection
/// usage, we are usually interested in the bytes buffered in the `bytes` field, which is
/// limited by the [MAX_BUFFER_SIZE]. For a handshake scenario, we are interested in the
/// actual Bluefin [packet](Self::packet), which contains important information for the handshake.
#[derive(Clone)]
pub(crate) struct ConnectionBuffer {
    ordered_bytes: OrderedBytes,
    addr: Option<SocketAddr>,
    waker: Option<Waker>,
    packet: Option<BluefinPacket>,
    dst_conn_id: u32,
    host_type: BluefinHost,
    set_start_packet_number: bool,
}

impl ConnectionBuffer {
    pub(crate) fn new(src_conn_id: u32, host_type: BluefinHost) -> Self {
        Self {
            ordered_bytes: OrderedBytes::new(src_conn_id, 0x0),
            addr: None,
            waker: None,
            packet: None,
            dst_conn_id: 0,
            host_type,
            set_start_packet_number: false,
        }
    }

    #[inline]
    pub(crate) fn set_dst_conn_id(&mut self, dst_conn_id: u32) {
        self.dst_conn_id = dst_conn_id;
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
    pub(crate) fn buffer_in_bytes(&mut self, packet: &BluefinPacket) -> BluefinResult<()> {
        self.ordered_bytes.buffer_in_packet(packet)
    }

    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: &BluefinPacket) -> BluefinResult<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError);
        }

        self.packet = Some(packet.clone());

        // We always set the start packet numbers once. For servers, we set in advance
        // that the start number is the first client hello we get + 2. (There is an ack)
        // For the client, we set it to + 1 (the next message we get should be data)
        if !self.set_start_packet_number {
            if self.host_type == BluefinHost::PackLeader {
                self.ordered_bytes
                    .set_start_packet_number(packet.header.packet_number + 2);
            } else if self.host_type == BluefinHost::Client {
                self.ordered_bytes
                    .set_start_packet_number(packet.header.packet_number + 1);
            }
            self.set_start_packet_number = true;
        }

        Ok(())
    }

    #[inline]
    pub(crate) fn consume(&mut self) -> BluefinResult<(Vec<u8>, SocketAddr)> {
        if self.addr.is_none() {
            return Err(BluefinError::Unexpected(
                "Cannot consume buffer because addr is field is none".to_string(),
            ));
        }

        let bytes = self.ordered_bytes.consume()?;
        let ans = (bytes, self.addr.unwrap());
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

/// ConnectionManager is what allows a single bluefin server to maintain multiple connections.
/// This struct is essentially a [mapping](Self::map) between a unique bidirectional connection key and its
/// connection buffer, which contains any bytes received during the connection. The unique key
/// has the form `{src_conn_id}_{dst_conn_id}`. If we are a client attempting to connect to a
/// server, then we do not know the dst_conn_id key. By protocol, the client must set the dst
/// id to 0x0.
/// This structure is used by all bluefin hosts to 'register' any new connections and is also
/// used by the reader TX worker to determine where to buffer a newly received packet.
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

    #[inline]
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

    #[inline]
    pub(crate) fn get(&self, key: &str) -> Option<Arc<Mutex<ConnectionBuffer>>> {
        self.map.get(key).cloned()
    }

    #[inline]
    pub(crate) fn remove(&mut self, key: &str) -> BluefinResult<()> {
        if self.map.remove(key).is_none() {
            return Err(BluefinError::NoSuchConnectionError);
        }
        Ok(())
    }
}

/// BluefinConnection represents a successful bluefin connection i.e. a bidirectional
/// connection established between a client and server after the handshake process
/// has completed successfully. A bluefin connection allows users to [receive](BluefinConnection::recv)
/// and to [send](BluefinConnection::send) bytes across the wire.
pub struct BluefinConnection {
    pub src_conn_id: u32,
    pub dst_conn_id: u32,
    // This is the *next* packet number we must use
    packet_num: u64,
    socket: Arc<UdpSocket>,
    rx: RxChannel,
}

impl BluefinConnection {
    pub(crate) fn new(
        src_conn_id: u32,
        dst_conn_id: u32,
        packet_num: u64,
        conn_buffer: Arc<Mutex<ConnectionBuffer>>,
        socket: Arc<UdpSocket>,
    ) -> Self {
        let rx = RxChannel::new(Arc::clone(&conn_buffer));
        Self {
            src_conn_id,
            dst_conn_id,
            packet_num,
            rx,
            socket,
        }
    }

    #[inline]
    pub async fn recv(&mut self, buf: &mut [u8]) -> BluefinResult<usize> {
        let (bytes, _) = self.rx.read().await;
        let size = buf.as_mut().write(&bytes)?;
        return Ok(size);
    }

    #[inline]
    pub async fn send(&mut self, buf: &[u8]) -> BluefinResult<usize> {
        // create bluefin packet and send
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            self.src_conn_id,
            self.dst_conn_id,
            PacketType::UnencryptedData,
            0x0,
            security_fields,
        );
        header.with_packet_number(self.packet_num);
        let packet = BluefinPacket::builder()
            .header(header)
            .payload(buf.to_vec())
            .build();
        let bytes = self.socket.send(&packet.serialise()).await?;

        self.packet_num += 1;

        Ok(bytes)
    }
}
