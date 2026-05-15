use std::{
    future::Future,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use bytes::Bytes;

use super::{
    build_and_start_ack_consumer_workers, build_and_start_conn_reader_tx_channels,
    close_handler::CloseBuffer,
    get_connected_udp_socket,
    ordered_bytes::{ConsumeResult, OrderedBytes},
    AckBuffer, ConnectionManagedBuffers, DiagSender, DiagnosticEvent, Wakeable,
};
use crate::{
    core::{header::BluefinSecurityFields, packet::BluefinPacket},
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

    /// Returns the peer's `SocketAddr` if it has been buffered yet (it
    /// is set during the handshake before any data flows). Used by
    /// [`crate::worker::reader::ReaderRxChannel`] to construct the EOF
    /// return tuple after the peer has sent us a `Fin`.
    #[inline]
    pub(crate) fn addr(&self) -> Option<SocketAddr> {
        self.addr
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
    /// Per-connection close-side state. Held here so [`Self::close`]
    /// can drive the FIN / FIN-ACK exchange and so a peer-initiated
    /// close can be observed via the public state accessors.
    close_buffer: Arc<Mutex<CloseBuffer>>,
    /// Host-wide connection registry. [`Self::close`] removes this
    /// connection's entry once the FIN / FIN-ACK exchange completes (or
    /// is force-closed after the retransmit budget) so the conn_reader
    /// stops routing further packets into a dead buffer.
    conn_manager: Arc<ConnectionManager>,
    diag_rx: Option<flume::Receiver<DiagnosticEvent>>,
    version: u8,
    security_fields: BluefinSecurityFields,
}

impl BluefinConnection {
    pub(crate) fn new(
        src_conn_id: u32,
        dst_conn_id: u32,
        next_send_packet_num: u64,
        conn_buffer: Arc<Mutex<ConnectionBuffer>>,
        ack_buffer: Arc<Mutex<AckBuffer>>,
        close_buffer: Arc<Mutex<CloseBuffer>>,
        peer_fin_observed: Arc<AtomicBool>,
        conn_manager: Arc<ConnectionManager>,
        dst_addr: SocketAddr,
        src_addr: SocketAddr,
        diag_tx: Option<DiagSender>,
    ) -> Self {
        // If diagnostics are enabled, create a bounded channel. The tx half
        // goes into the internal workers (ConnReaderHandler + ReaderRxChannel);
        // the rx half stays on the connection for the caller to poll.
        let (diag_tx_workers, diag_rx) = if diag_tx.is_some() {
            let (tx, rx) = flume::bounded::<DiagnosticEvent>(256);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

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
        writer_handler.diag_tx = diag_tx_workers.clone();
        if let Err(e) = writer_handler.start() {
            panic!("Cannot start connection due to error: {:?}", e);
        }

        // Spawn the FinAck-send drainer. The conn_reader sends a packet
        // number on `fin_ack_tx` whenever a peer `Fin` arrives; this task
        // forwards it to `WriterHandler::send_fin_ack` which formats and
        // emits a header-only FIN-ACK datagram on the per-connection
        // socket. Bounded so a flood of duplicate FINs cannot grow the
        // queue without bound; the drainer keeps up easily since each
        // emit is a single 20-byte try_send.
        let (fin_ack_tx, fin_ack_rx) = flume::bounded::<u64>(8);
        {
            let writer_for_finack = writer_handler.clone();
            tokio::spawn(async move {
                while let Ok(pn) = fin_ack_rx.recv_async().await {
                    let _ = writer_for_finack.send_fin_ack(pn).await;
                }
            });
        }

        let conn_bufs = Arc::new(ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buffer),
            close_buff: Arc::clone(&close_buffer),
            peer_fin_observed: Arc::clone(&peer_fin_observed),
            fin_ack_tx: Some(fin_ack_tx),
            diag_tx: diag_tx_workers.clone(),
        });

        let _ = build_and_start_conn_reader_tx_channels(Arc::clone(&conn_socket), conn_bufs);

        let reader_rx = ReaderRxChannel::new(
            Arc::clone(&conn_buffer),
            Arc::clone(&peer_fin_observed),
            writer_handler.clone(),
            diag_tx_workers,
        );

        Self {
            src_conn_id,
            dst_conn_id,
            reader_rx,
            writer_handler,
            close_buffer: Arc::clone(&close_buffer),
            conn_manager,
            diag_rx,
            version: 0x0,
            security_fields: BluefinSecurityFields::default(),
        }
    }

    /// Reads up to `len` bytes from the connection into `buf`. Returns the
    /// number of bytes actually read.
    ///
    /// Returns `Ok(0)` when the peer has gracefully closed the connection
    /// with a `Fin` and the receive buffer has been fully drained
    /// (end-of-stream). Subsequent calls also return `Ok(0)`.
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

    /// Initiates a graceful close of this connection per bluefin-protocol
    /// §10bis (FIN / FIN-ACK exchange).
    ///
    /// Sequence:
    /// 1. [`Self::flush`] — drain every byte already accepted by `send_*`
    ///    onto the wire.
    /// 2. Mark the writer closed so any further `send_*` returns
    ///    [`BluefinError::ConnectionClosed`].
    /// 3. Reserve the FIN's packet number (the next available in the
    ///    sender's space, post-flush) and emit a header-only `Fin`.
    /// 4. Park on the close-buffer's notify until the peer's matching
    ///    `FinAck` arrives, with a 200 ms idle timeout. On timeout,
    ///    retransmit the `Fin` (up to 3 attempts total). After the budget
    ///    is exhausted the connection is force-closed locally and
    ///    [`BluefinError::TimedOut`] is returned.
    /// 5. Mark the close buffer `Closed` and return.
    ///
    /// Idempotent on the writer side (a second call sees the closed flag
    /// already set), but a second call MAY still send another `Fin` —
    /// callers SHOULD invoke this exactly once per connection.
    pub async fn close(&self) -> BluefinResult<()> {
        // Drain the writer pipeline first so the FIN is strictly ordered
        // after every previously-accepted data byte.
        self.writer_handler.flush().await?;

        // Forbid further data sends. Done BEFORE reserving the FIN's
        // packet number so a racing send cannot claim a number after ours.
        self.writer_handler.mark_closed();

        // Reserve the FIN's packet number. After flush() returned, the
        // writer task is idle and the shared atomic mirrors the writer's
        // local counter accurately. `fetch_add(1)` reserves our pn and
        // bumps the counter for any future caller (defensive).
        let fin_pn = self.writer_handler.reserve_close_packet_num();

        // Record locally that we have initiated close, then send.
        {
            let mut g = self.close_buffer.lock().unwrap();
            g.record_local_fin_sent(fin_pn);
        }

        // Take the notify handle out before the retransmit loop so we
        // don't hold the close-buffer mutex across `await`.
        let notify = self.close_buffer.lock().unwrap().fin_ack_notify();

        const MAX_FIN_ATTEMPTS: usize = 3;
        const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(200);

        for _attempt in 0..MAX_FIN_ATTEMPTS {
            self.writer_handler.send_fin(fin_pn).await?;

            // Standard `Notify` double-check pattern: register interest
            // before re-reading the state so a notify that fires between
            // the read and the await is not lost.
            let notified = notify.notified();
            if self.close_buffer.lock().unwrap().received_fin_ack_for() == Some(fin_pn) {
                self.close_buffer.lock().unwrap().mark_closed();
                self.deregister_from_conn_manager();
                return Ok(());
            }
            match tokio::time::timeout(PER_ATTEMPT_TIMEOUT, notified).await {
                Ok(_) => {
                    if self.close_buffer.lock().unwrap().received_fin_ack_for() == Some(fin_pn) {
                        self.close_buffer.lock().unwrap().mark_closed();
                        self.deregister_from_conn_manager();
                        return Ok(());
                    }
                    // Spurious wake — fall through to the next attempt
                    // (which will retransmit the FIN).
                }
                Err(_) => {
                    // Per-attempt timeout — retransmit on the next loop.
                }
            }
        }

        // Force-close locally and surface the timeout to the caller.
        self.close_buffer.lock().unwrap().mark_closed();
        self.deregister_from_conn_manager();
        Err(BluefinError::TimedOut(
            "close: peer did not send FinAck within retransmit budget",
        ))
    }

    /// Remove this connection's entry from the host-wide
    /// [`ConnectionManager`]. After this returns, the conn_reader stops
    /// routing further inbound packets into our buffers. Safe to call
    /// more than once: `DashMap::remove` is a no-op on missing keys.
    #[inline]
    fn deregister_from_conn_manager(&self) {
        let _ = self
            .conn_manager
            .remove(&(self.src_conn_id, self.dst_conn_id));
    }

    /// Returns `true` once this connection has been fully closed (either
    /// because [`Self::close`] completed locally, or because the peer's
    /// `Fin` arrived and the FIN/FIN-ACK exchange has been driven to
    /// completion).
    #[inline]
    pub fn is_closed(&self) -> bool {
        use crate::net::close_handler::CloseState;
        matches!(
            self.close_buffer.lock().unwrap().state(),
            CloseState::Closed
        )
    }

    /// Returns a reference to the diagnostic event receiver, if one was
    /// wired up at connection time. Use `try_recv()` to poll for events
    /// without blocking.
    #[inline]
    pub fn diag_rx(&self) -> Option<&flume::Receiver<DiagnosticEvent>> {
        self.diag_rx.as_ref()
    }

    /// Protocol version negotiated for this connection.
    #[inline]
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Security fields (encrypted flag + header-protection mask).
    #[inline]
    pub fn security_fields(&self) -> &BluefinSecurityFields {
        &self.security_fields
    }
}
