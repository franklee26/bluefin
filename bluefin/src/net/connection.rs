use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use bytes::Bytes;

use super::{
    build_and_start_ack_consumer_workers, build_and_start_conn_reader_tx_channels,
    get_connected_udp_socket,
    ordered_bytes::{ConsumeResult, OrderedBytes},
    AckBuffer, ConnectionManagedBuffers, Wakeable,
};
use crate::{
    core::packet::BluefinPacket,
    worker::{reader::ReaderRxChannel, writer::WriterHandler},
};
use bluefin_proto::context::BluefinHost;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use tokio::time::timeout;

pub const MAX_BUFFER_SIZE: usize = 10_000;

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

    /// Awaits the future for a handshake-related packet stored in the [HandshakeConnectionBuffer::conn_buff].
    #[inline]
    pub(crate) async fn read(&self) -> (BluefinPacket, SocketAddr) {
        self.clone().await
    }

    /// Awaits the future for a handshake-related packet stored in the [HandshakeConnectionBuffer::conn_buff].
    /// This does the same thing as [read](Self::read) but this will return a timeout error if the future does
    /// not yield a result after the specified duration.
    #[inline]
    pub(crate) async fn read_with_timeout(
        &self,
        timeout_duration: Duration,
    ) -> BluefinResult<(BluefinPacket, SocketAddr)> {
        if let Ok(res) = timeout(timeout_duration, self.clone()).await {
            return Ok(res);
        }

        Err(BluefinError::TimedOut(
            "Failed to read from handshake connection buffer",
        ))
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
        guard.set_waker_if_changed(cx.waker());
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
    pub(crate) fn buffer_in_bytes(&mut self, packet: BluefinPacket) -> BluefinResult<()> {
        self.ordered_bytes.buffer_in_packet(packet)
    }

    #[inline]
    pub(crate) fn buffer_in_packet(&mut self, packet: BluefinPacket) -> BluefinResult<()> {
        if self.packet.is_some() {
            return Err(BluefinError::BufferFullError(
                "Buffer already contains a packet. Could not buffer another packet.".to_string(),
            ));
        }

        let packet_num = packet.header.packet_number;
        self.packet = Some(packet);

        // We always set the start packet numbers once. For servers, we set in advance
        // that the start number is the first client hello we get + 2. (There is an ack)
        // For the client, we set it to + 1 (the next message we get should be data)
        if !self.set_start_packet_number {
            if self.host_type == BluefinHost::PackLeader {
                self.ordered_bytes.set_start_packet_number(packet_num + 2);
            } else if self.host_type == BluefinHost::Client {
                self.ordered_bytes.set_start_packet_number(packet_num + 1);
            }
            self.set_start_packet_number = true;
        }

        Ok(())
    }

    #[inline]
    pub(crate) fn consume(
        &mut self,
        bytes_to_read: usize,
        buf: &mut [u8],
    ) -> BluefinResult<(ConsumeResult, SocketAddr)> {
        if self.addr.is_none() {
            return Err(BluefinError::Unexpected(
                "Cannot consume buffer because addr is field is none".to_string(),
            ));
        }

        let consume_res = self.ordered_bytes.consume(bytes_to_read, buf)?;
        Ok((consume_res, self.addr.unwrap()))
    }

    /// Zero-copy variant of [`Self::consume`]. Hands back whole-payload
    /// [`Bytes`] slices over the recv buffer instead of memcpying into
    /// a caller `&mut [u8]`. See [`OrderedBytes::consume_bytes`].
    #[inline]
    pub(crate) fn consume_bytes(
        &mut self,
        out: &mut Vec<Bytes>,
        max_packets: usize,
    ) -> BluefinResult<(ConsumeResult, SocketAddr)> {
        if self.addr.is_none() {
            return Err(BluefinError::Unexpected(
                "Cannot consume buffer because addr is field is none".to_string(),
            ));
        }

        let consume_res = self.ordered_bytes.consume_bytes(out, max_packets)?;
        Ok((consume_res, self.addr.unwrap()))
    }

    pub(crate) fn peek(&self) -> BluefinResult<()> {
        if self.addr.is_none() {
            return Err(BluefinError::Unexpected(
                "Cannot consume buffer because addr is field is none".to_string(),
            ));
        }
        self.ordered_bytes.peek()
    }

    #[inline]
    pub(crate) fn get_waker(&self) -> Option<&Waker> {
        self.waker.as_ref()
    }

    #[inline]
    pub(crate) fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker);
    }

    /// Sets the waker only if it's different from the current one.
    /// This avoids unnecessary cloning when the same task is polling repeatedly.
    #[inline]
    pub(crate) fn set_waker_if_changed(&mut self, new_waker: &Waker) {
        if let Some(ref existing) = self.waker {
            if existing.will_wake(new_waker) {
                return; // Same waker, no need to clone
            }
        }
        self.waker = Some(new_waker.clone());
    }
}

impl Wakeable for ConnectionBuffer {
    #[inline]
    fn take_waker_clone(&self) -> Option<Waker> {
        self.waker.clone()
    }
}

/// ConnectionManager is what allows a single bluefin server to maintain multiple connections.
/// This is a lock-free concurrent mapping between a unique bidirectional connection key and its
/// connection buffer, which contains any bytes received during the connection. The unique key
/// has the form `{src_conn_id}_{dst_conn_id}`. If we are a client attempting to connect to a
/// server, then we do not know the dst_conn_id key. By protocol, the client must set the dst
/// id to 0x0.
/// This structure is used by all bluefin hosts to 'register' any new connections and is also
/// used by the reader TX worker to determine where to buffer a newly received packet.
///
/// Uses DashMap for lock-free concurrent access - eliminates Arc<Mutex<HashMap>> contention.
/// Key: (src_conn_id, dst_conn_id)
/// Value: The connection buffer
pub(crate) type ConnectionManager = dashmap::DashMap<(u32, u32), ConnectionManagedBuffers>;

/// BluefinConnection represents a successful bluefin connection i.e. a bidirectional
/// connection established between a client and server after the handshake process
/// has completed successfully. A bluefin connection allows users to [receive](BluefinConnection::recv)
/// and to [send](BluefinConnection::send) bytes across the wire.
#[derive(Clone)]
pub struct BluefinConnection {
    pub src_conn_id: u32,
    pub dst_conn_id: u32,
    reader_rx: ReaderRxChannel,
    writer_handler: WriterHandler,
}

impl BluefinConnection {
    pub(crate) fn new(
        src_conn_id: u32,
        dst_conn_id: u32,
        next_send_packet_num: u64,
        conn_buffer: Arc<Mutex<ConnectionBuffer>>,
        ack_buffer: Arc<Mutex<AckBuffer>>,
        dst_addr: SocketAddr,
        src_addr: SocketAddr,
    ) -> Self {
        build_and_start_ack_consumer_workers(1, Arc::clone(&ack_buffer));
        let s = get_connected_udp_socket(src_addr, dst_addr);
        if let Err(e) = s {
            panic!("Failed to get connected sockets due to error: {:?}", e);
        }
        let conn_socket = Arc::new(s.unwrap());

        let mut writer_handler = WriterHandler::new(
            Arc::clone(&conn_socket),
            next_send_packet_num,
            src_conn_id,
            dst_conn_id,
        );
        if let Err(e) = writer_handler.start() {
            panic!("Cannot start connection due to error: {:?}", e);
        }

        let conn_bufs = Arc::new(ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buffer),
        });

        let _ = build_and_start_conn_reader_tx_channels(Arc::clone(&conn_socket), conn_bufs);

        let reader_rx = ReaderRxChannel::new(Arc::clone(&conn_buffer), writer_handler.clone());

        Self {
            src_conn_id,
            dst_conn_id,
            reader_rx,
            writer_handler,
        }
    }

    #[inline]
    pub async fn recv(&mut self, buf: &mut [u8], len: usize) -> BluefinResult<usize> {
        let (size, _) = self.reader_rx.read(len, buf).await?;
        Ok(size as usize)
    }

    /// Zero-copy variant of [`Self::recv`]. Pushes up to `max_packets`
    /// whole-payload [`Bytes`] slices into `out` instead of memcpying
    /// into a caller `&mut [u8]`. Each pushed `Bytes` is a refcount view
    /// over the recv buffer, so this avoids the `_platform_memmove`
    /// in `OrderedBytes::consume`.
    ///
    /// Returns the total number of payload bytes pushed into `out` on
    /// this call. The vec is *not* cleared on entry — drain or clear it
    /// yourself between calls. Reusing the same vec preserves capacity
    /// and avoids per-call allocation.
    ///
    /// Use [`Self::recv`] if you need an owned contiguous buffer (e.g.
    /// to feed a parser that doesn't accept `Bytes` chunks). Use this if
    /// the consumer can work directly with a stream of `Bytes` slices —
    /// it's strictly cheaper.
    #[inline]
    pub async fn recv_bytes(
        &mut self,
        out: &mut Vec<Bytes>,
        max_packets: usize,
    ) -> BluefinResult<usize> {
        let (size, _) = self.reader_rx.read_bytes(out, max_packets).await?;
        Ok(size as usize)
    }

    #[inline]
    pub fn send(&mut self, buf: &[u8]) -> BluefinResult<usize> {
        self.writer_handler.send_data(buf)
    }

    /// Send an owned [`Bytes`]. Faster than [`Self::send`] when the caller
    /// already holds a `Bytes` (e.g. from a buffer pool or via
    /// `Bytes::clone()`), because the writer pipeline carries `Bytes`
    /// internally and a clone is just a refcount bump.
    ///
    /// Synchronous: returns an error if the writer's bounded send queue is
    /// full. Use [`Self::send_bytes_async`] on the hot path of high-throughput
    /// producers; it awaits backpressure instead, so callers don't need to
    /// sleep at the end of the run waiting for the queue to drain.
    #[inline]
    pub fn send_bytes(&mut self, payload: Bytes) -> BluefinResult<usize> {
        self.writer_handler.send_bytes(payload)
    }

    /// Async variant of [`Self::send_bytes`] that awaits backpressure when
    /// the writer's send queue is full. Preferred for tight high-throughput
    /// loops because the caller and the writer task naturally synchronise:
    /// once the loop returns, the writer has drained roughly everything that
    /// was enqueued, so no end-of-run drain sleep is needed.
    #[inline]
    pub async fn send_bytes_async(&mut self, payload: Bytes) -> BluefinResult<usize> {
        self.writer_handler.send_bytes_async(payload).await
    }

    /// Awaits until every byte previously accepted by [`Self::send`],
    /// [`Self::send_bytes`], or [`Self::send_bytes_async`] has actually
    /// been written to the underlying socket.
    ///
    /// This is the only correct way to drain the writer pipeline before
    /// dropping the connection or exiting the process. Prior to this API,
    /// callers (including the bench client) had to fall back on a fixed
    /// `tokio::time::sleep` and hope it was long enough \u2014 visibly wrong
    /// under load. With `flush().await`, the wait is exactly as long as
    /// the writer needs and never longer.
    ///
    /// Returns immediately if there is nothing pending. Cheap to call.
    #[inline]
    pub async fn flush(&self) -> BluefinResult<()> {
        self.writer_handler.flush().await
    }
}
