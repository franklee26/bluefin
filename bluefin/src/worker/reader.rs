use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::Poll,
};

use super::writer::WriterHandler;
use crate::{
    core::{header::PacketType, packet::BluefinPacket},
    net::{
        ack_handler::AckBuffer,
        connection::{ConnectionBuffer, ConnectionManager},
        diag_try_send, ConnectionManagedBuffers,
        DiagSender, DiagnosticEvent, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM,
    },
};
use bluefin_proto::context::BluefinHost;
use bluefin_proto::endpoint::{is_client_ack_packet, Endpoint, HelloOutcome};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::BytesMut;
use tokio::net::UdpSocket;

#[derive(Clone)]
/// [ReaderTxChannel] is the transmission channel for the receiving [ReaderRxChannel]. This channel will when
/// [run](Self::run), asynchronously read from the udp socket and upon receiving a packet, the channel
/// attempts to serialise it to a bluefin packet. If a bluefin packet is found then the channel will
/// use the [conn_manager](Self::conn_manager) to identify the correct connection buffer and attempt
/// to buffer in the bytes/packet. In other words, this channel *transmits* bytes *into* the buffer
/// and signals any awaiters that data is ready.
pub(crate) struct ReaderTxChannel {
    pub(crate) id: u16,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<ConnectionManager>,
    endpoint: Arc<Mutex<Endpoint>>,
    host_type: BluefinHost,
}

#[derive(Clone)]
/// [ReaderRxChannel] is the receiving channel for the transmitting [ReaderRxChannel]. This channel will when
/// [read](Self::read), asynchronously peek into [Self::buffer] and will eventually return the
/// buffered tuple contents ([ConsumeResult], [SocketAddr]). In other words, this channel
/// *receives* bytes *from* the buffer.
pub(crate) struct ReaderRxChannel {
    future: ReaderRxChannelFuture,
    peer_fin_observed: Arc<AtomicBool>,
    writer_handler: WriterHandler,
    packets_consumed: usize,
    packets_consumed_before_ack: usize,
    diag_tx: Option<DiagSender>,
}

#[derive(Clone)]
struct ReaderRxChannelFuture {
    buffer: Arc<Mutex<ConnectionBuffer>>,
    /// Lock-free mirror of `CloseBuffer.peer_fin_observed`. When the peer
    /// has sent us a `Fin` and the data buffer is drained, the future
    /// resolves so [`ReaderRxChannel::read`] can return EOF (`Ok(0)`)
    /// instead of parking indefinitely.
    peer_fin_observed: Arc<AtomicBool>,
}

// NOTE: an earlier round (G, reverted) tried to merge the consumer's two
// lock acquisitions — the brief peek-only one in `poll` and the longer
// consume one in `read` — into a single peek+consume under the same lock.
// That regressed peak throughput from 4.30 GB/s → 3.56 GB/s. The two-lock
// shape is actually advantageous: the producer (`recv_and_buffer_inline`)
// can grab the lock during the brief gap between the consumer's peek and
// consume, so the consumer's longer consume hold doesn't fully starve the
// producer's recv-loop fast path. Don't re-collapse without measuring.
impl Future for ReaderRxChannelFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.buffer.lock().unwrap();
        if let Ok(()) = guard.peek() {
            return Poll::Ready(());
        }

        // Peer has sent us a FIN and the data buffer is empty. Wake the
        // caller so `read*` can return EOF instead of parking forever.
        // See bluefin-protocol §10bis and `net::close_handler`.
        if self.peer_fin_observed.load(Ordering::Acquire) {
            return Poll::Ready(());
        }

        guard.set_waker_if_changed(cx.waker());
        Poll::Pending
    }
}

impl ReaderRxChannel {
    pub(crate) fn new(
        buffer: Arc<Mutex<ConnectionBuffer>>,
        peer_fin_observed: Arc<AtomicBool>,
        writer_handler: WriterHandler,
        diag_tx: Option<DiagSender>,
    ) -> Self {
        let future = ReaderRxChannelFuture {
            buffer,
            peer_fin_observed: Arc::clone(&peer_fin_observed),
        };
        Self {
            future,
            peer_fin_observed,
            writer_handler,
            packets_consumed: 0,
            packets_consumed_before_ack: 200,
            diag_tx,
        }
    }

    #[inline]
    pub(crate) async fn read(
        &mut self,
        bytes_to_read: usize,
        buf: &mut [u8],
    ) -> BluefinResult<(u64, SocketAddr)> {
        let _ = self.future.clone().await;
        // EOF fast path: if the peer has sent us a `Fin` and there is
        // nothing left to consume, surface (0, addr) so the caller sees
        // a clean end-of-stream. We need the buffer's `addr` for the
        // return tuple; it was set during the handshake and is stable
        // for the lifetime of the connection.
        if self.peer_fin_observed.load(Ordering::Acquire) {
            let guard = self.future.buffer.lock().unwrap();
            if guard.peek().is_err() {
                if let Some(addr) = guard.addr() {
                    return Ok((0, addr));
                }
            }
        }
        // Minimize lock scope - only hold lock during consume operation
        let (consume_res, addr) = {
            self.future.buffer.lock().unwrap().consume(bytes_to_read, buf).unwrap()
        };
        let num_packets_consumed = consume_res.get_num_packets_consumed();
        let base_packet_num = consume_res.get_base_packet_number();
        self.packets_consumed += num_packets_consumed;

        if num_packets_consumed > 0 {
            diag_try_send(
                &self.diag_tx,
                DiagnosticEvent::DataReceived {
                    base_packet_num,
                    num_packets: num_packets_consumed,
                    num_bytes: consume_res.get_bytes_consumed() as usize,
                },
            );
        }

        // We need to send an ack.
        if num_packets_consumed > 0
            && base_packet_num != 0
            && self.packets_consumed >= self.packets_consumed_before_ack
        {
            let _ = self
                .writer_handler
                .send_ack(base_packet_num, num_packets_consumed);
            diag_try_send(
                &self.diag_tx,
                DiagnosticEvent::AckSent {
                    base_packet_num,
                    count: num_packets_consumed,
                },
            );
            self.packets_consumed = 0;
        }

        Ok((consume_res.get_bytes_consumed(), addr))
    }

    /// Zero-copy variant of [`Self::read`]. Hands back whole-payload
    /// [`Bytes`] slices via `out` instead of memcpying into a caller
    /// `&mut [u8]`. Same await + lock + ack-bookkeeping shape as `read`.
    /// See [`ConnectionBuffer::consume_bytes`] /
    /// [`OrderedBytes::consume_bytes`].
    #[inline]
    pub(crate) async fn read_bytes(
        &mut self,
        out: &mut Vec<bytes::Bytes>,
        max_packets: usize,
    ) -> BluefinResult<(u64, SocketAddr)> {
        let _ = self.future.clone().await;
        // EOF fast path — see `read` above.
        if self.peer_fin_observed.load(Ordering::Acquire) {
            let guard = self.future.buffer.lock().unwrap();
            if guard.peek().is_err() {
                if let Some(addr) = guard.addr() {
                    return Ok((0, addr));
                }
            }
        }
        let (consume_res, addr) = {
            self.future
                .buffer
                .lock()
                .unwrap()
                .consume_bytes(out, max_packets)?
        };
        let num_packets_consumed = consume_res.get_num_packets_consumed();
        let base_packet_num = consume_res.get_base_packet_number();
        self.packets_consumed += num_packets_consumed;

        if num_packets_consumed > 0 {
            diag_try_send(
                &self.diag_tx,
                DiagnosticEvent::DataReceived {
                    base_packet_num,
                    num_packets: num_packets_consumed,
                    num_bytes: consume_res.get_bytes_consumed() as usize,
                },
            );
        }

        if num_packets_consumed > 0
            && base_packet_num != 0
            && self.packets_consumed >= self.packets_consumed_before_ack
        {
            let _ = self
                .writer_handler
                .send_ack(base_packet_num, num_packets_consumed);
            diag_try_send(
                &self.diag_tx,
                DiagnosticEvent::AckSent {
                    base_packet_num,
                    count: num_packets_consumed,
                },
            );
            self.packets_consumed = 0;
        }

        Ok((consume_res.get_bytes_consumed(), addr))
    }
}

/// Result of checking whether a packet is a hello that should be
/// handled specially by the handshake path.
enum HelloAction {
    /// Not a hello packet — proceed with normal data routing.
    NotHello,
    /// Hello was matched to a pending `accept()` slot.
    /// The `u32` is the server-side connection ID to route to.
    Routed(u32),
    /// No `accept()` slot was ready — packet has been queued (or
    /// dropped if the queue is full). Caller should `continue`.
    Queued,
}

impl ReaderTxChannel {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        conn_manager: Arc<ConnectionManager>,
        endpoint: Arc<Mutex<Endpoint>>,
        host_type: BluefinHost,
    ) -> Self {
        Self {
            id: 0,
            socket,
            conn_manager,
            endpoint,
            host_type,
        }
    }

    /// Checks whether the single packet in `packets` is a hello and, if
    /// so, either routes it to a pending `accept()` slot or queues it for
    /// a future `accept()` — all by delegating to the sans-io
    /// [`Endpoint::classify_hello`]. The mapping from `HelloOutcome` to
    /// the runtime's `HelloAction` is mechanical; the runtime just
    /// collapses `Queued`/`Dropped` (both "caller discards the packet")
    /// into a single `Queued` variant.
    #[inline]
    fn handle_hello_single(
        &self,
        packets: &mut Vec<BluefinPacket>,
        addr: SocketAddr,
    ) -> HelloAction {
        match self.endpoint.lock().unwrap().classify_hello(packets, addr) {
            HelloOutcome::NotHello => HelloAction::NotHello,
            HelloOutcome::Routed { our_id } => HelloAction::Routed(our_id),
            HelloOutcome::Queued | HelloOutcome::Dropped => HelloAction::Queued,
        }
    }

    #[inline]
    fn build_conn_buff_key(is_hello: bool, src_conn_id: u32, dst_conn_id: u32) -> (u32, u32) {
        if !is_hello {
            (src_conn_id, dst_conn_id)
        } else {
            (src_conn_id, 0)
        }
    }

    fn buffer_to_conn_buffer(
        conn_buff: &mut ConnectionBuffer,
        packet: BluefinPacket,
        addr: SocketAddr,
        is_hello: bool,
        is_client_ack: bool,
    ) -> BluefinResult<()> {
        let packet_src_conn_id = packet.header.source_connection_id;
        if !is_hello && !is_client_ack {
            // If not hello, we buffer in the bytes
            conn_buff.buffer_in_bytes(packet)?;
        } else {
            conn_buff.buffer_in_packet(packet)?;
            let _ = conn_buff.buffer_in_addr(addr);
        }

        conn_buff.set_dst_conn_id(packet_src_conn_id);

        // Wake future that buffered data is available
        if let Some(w) = conn_buff.get_waker() {
            w.wake_by_ref();
        } else {
            return Err(BluefinError::NoSuchWakerError);
        }
        Ok(())
    }

    #[inline]
    fn buffer_to_ack_buffer(ack_buff: &mut AckBuffer, packet: BluefinPacket) -> BluefinResult<()> {
        ack_buff.buffer_in_ack_packet(packet)?;
        ack_buff.wake()
    }

    #[inline]
    fn buffer_in_data(
        is_hello: bool,
        host_type: BluefinHost,
        packet: BluefinPacket,
        addr: SocketAddr,
        buffers: &ConnectionManagedBuffers,
    ) -> BluefinResult<()> {
        let is_client_ack = is_client_ack_packet(host_type, &packet);
        if !is_client_ack && !is_hello && packet.header.type_field == PacketType::Ack {
            let mut ack_buff = buffers.ack_buff.lock().unwrap();
            Self::buffer_to_ack_buffer(&mut ack_buff, packet)?;
        } else {
            let mut conn_buff = buffers.conn_buff.lock().unwrap();
            Self::buffer_to_conn_buffer(&mut conn_buff, packet, addr, is_hello, is_client_ack)?;
        }
        Ok(())
    }

    /// The [TxChannel]'s engine runner. This method will run forever and is responsible for reading bytes
    /// from the udp socket into a connection buffer. This method should be run its own asynchronous task.
    pub(crate) async fn run(&mut self) -> BluefinResult<()> {
        // Pre-allocate packet buffer to reuse across iterations (eliminates Vec allocation overhead)
        let mut packets = Vec::with_capacity(76); // Max packets per datagram

        loop {
            // One heap allocation per recv (see `conn_reader::tx_impl` for
            // the rationale). The freezed `Bytes` is sliced inside
            // `from_bytes_into` so each parsed payload is a refcount view
            // over this single buffer rather than its own allocation.
            let mut buf = BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
            // SAFETY: recv writes `size` bytes; `truncate(size)` immediately
            // discards the still-uninit tail. Mirrors the previous
            // `MaybeUninit<[u8; MAX]>` idiom this loop used.
            unsafe { buf.set_len(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM); }

            // Try non-blocking recv first for lower latency when packets are queued
            let (size, addr) = match self.socket.try_recv_from(&mut buf[..]) {
                Ok(result) => result,
                Err(_) => self.socket.recv_from(&mut buf[..]).await?,
            };
            buf.truncate(size);
            let frozen = buf.freeze();

            // Zero-copy packet parsing: each packet's payload is a refcount
            // view over `frozen`.
            packets.clear();
            if BluefinPacket::from_bytes_into(frozen, &mut packets).is_err() {
                continue;
            }

            if packets.is_empty() {
                continue;
            }

            // Copy the header data we need BEFORE any borrowing issues
            // Because all bluefin packets bundled in a datagram must come from the same host, we just peek
            // at the first one
            let first_pkt_hdr = packets[0].header;
            let mut src_conn_id = first_pkt_hdr.destination_connection_id;
            let dst_conn_id = first_pkt_hdr.source_connection_id;
            let mut is_hello = false;

            // If there is only one packet, then it's possible it is a handshake packet. Handshakes are sent
            // via one udp datagram carries exactly one bluefin packet
            if packets.len() == 1 {
                match self.handle_hello_single(&mut packets, addr) {
                    HelloAction::Routed(id) => {
                        if self.host_type == BluefinHost::PackLeader {
                            src_conn_id = id;
                        }
                        is_hello = true;
                    }
                    HelloAction::NotHello => { /* fall through to normal routing */ }
                    HelloAction::Queued => { continue; }
                }
            }

            let key = ReaderTxChannel::build_conn_buff_key(is_hello, src_conn_id, dst_conn_id);
            let _conn_buf = {
                // Lock-free lookup with DashMap - no contention!
                self.conn_manager
                    .get(&key)
                    .map(|entry| entry.value().clone())
            };

            if _conn_buf.is_none() {
                continue;
            }

            let buffers = _conn_buf.unwrap();
            // Use drain to consume packets while keeping the Vec allocated for next iteration
            for p in packets.drain(..) {
                let _ =
                    ReaderTxChannel::buffer_in_data(is_hello, self.host_type, p, addr, &buffers);
            }
            // packets is now empty but still has its capacity - will be reused in next iteration
        }
    }
}
