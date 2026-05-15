use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::Waker;

use crate::{
    core::{
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
    },
    utils::get_connected_udp_socket,
    worker::{conn_reader::ConnReaderHandler, reader::ReaderTxChannel},
};
use ack_handler::{AckBuffer, AckConsumer};
use bluefin_proto::context::BluefinHost;
use bluefin_proto::BluefinResult;
use connection::{ConnectionBuffer, ConnectionManager};
use tokio::{net::UdpSocket, spawn};

pub mod ack_handler;
pub mod client;
pub mod connection;
pub mod ordered_bytes;
pub mod server;

/// Maximum number of `ClientHello` packets to buffer when no `accept()` slot
/// is ready yet. Prevents unbounded memory growth from a flood of hellos.
pub(crate) const MAX_QUEUED_HELLOS: usize = 64;

/// Shared state between the server's `accept()` and the `ReaderTxChannel`
/// workers. Protected by a single mutex so that checking `pending_accept_ids`
/// and pushing to / popping from `hello_queue` is atomic — no TOCTOU race.
pub(crate) struct HelloState {
    /// Accept slots that are ready for incoming hellos (FIFO).
    pub(crate) pending_accept_ids: VecDeque<u32>,
    /// `ClientHello` packets that arrived before their `accept()` slot existed.
    pub(crate) hello_queue: VecDeque<(BluefinPacket, SocketAddr)>,
}

impl HelloState {
    pub(crate) fn new() -> Self {
        Self {
            pending_accept_ids: VecDeque::new(),
            hello_queue: VecDeque::new(),
        }
    }
}

pub(crate) const BLUEFIN_HEADER_SIZE_BYTES: usize = 20;
pub(crate) const MAX_BLUEFIN_PAYLOAD_SIZE_BYTES: usize = 1500;
pub(crate) const MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM: usize = 10;
pub(crate) const MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM: usize = MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM
    * (BLUEFIN_HEADER_SIZE_BYTES + MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);

// ---------------------------------------------------------------------------
// Diagnostic events (opt-in, zero cost when disabled)
// ---------------------------------------------------------------------------

/// Events emitted by the Bluefin runtime when an optional diagnostic channel
/// is wired up. The channel is `Option<flume::Sender<DiagnosticEvent>>` so
/// there is zero overhead when diagnostics are disabled.
#[derive(Debug, Clone)]
pub enum DiagnosticEvent {
    /// We received ACK packets from the peer (the peer is acknowledging
    /// data we sent). `base_packet_num` is the first packet number in the
    /// contiguous range and `count` is how many packets were acknowledged.
    AckReceived {
        base_packet_num: u64,
        count: usize,
    },
    /// We sent an ACK to the peer (acknowledging data we received).
    /// `base_packet_num` + `count` describe the contiguous range we acked.
    AckSent {
        base_packet_num: u64,
        count: usize,
    },
    /// Data packets were serialized and queued for sending.
    /// `start_packet_num` is the first packet number assigned;
    /// `num_packets` is how many packets were created; `num_bytes` is the
    /// total user-payload bytes packed.
    DataSent {
        start_packet_num: u64,
        num_packets: u64,
        num_bytes: usize,
    },
    /// Data packets were consumed from the receive buffer.
    /// `base_packet_num` is the first packet number in the consumed range;
    /// `num_packets` is how many packets were consumed; `num_bytes` is the
    /// total payload bytes delivered.
    DataReceived {
        base_packet_num: u64,
        num_packets: usize,
        num_bytes: usize,
    },
}

pub(crate) type DiagSender = flume::Sender<DiagnosticEvent>;

/// Best-effort send: if the channel is full or disconnected we silently drop.
#[inline]
pub(crate) fn diag_try_send(tx: &Option<DiagSender>, event: DiagnosticEvent) {
    if let Some(ref sender) = tx {
        let _ = sender.try_send(event);
    }
}

/// Implemented by every buffer type that owns an `Option<Waker>` and is
/// shared between a producer task and a consumer task via `Arc<Mutex<...>>`.
///
/// The contract for producers is always:
///
/// 1. Lock the buffer.
/// 2. Mutate it (push a packet, advance a counter, etc.).
/// 3. Call [`Wakeable::take_waker_clone`] to lift a `Waker` clone out.
/// 4. **Drop the mutex guard.**
/// 5. Call `wake()` (or `wake_by_ref()`) on the cloned waker.
///
/// Steps 3–5 — in that order — are what stop the woken consumer task from
/// immediately bouncing on `lock()` while the producer is still holding the
/// guard. See `bluefin-architecture` §5 (the buffer-with-waker pattern) and
/// the `bluefin-performance` historical timeline (#7).
pub(crate) trait Wakeable {
    /// Returns a clone of the stored `Waker`, or `None` if no consumer has
    /// registered one yet.
    ///
    /// Cloning a `Waker` is cheap — internally it bumps an atomic refcount
    /// on a vtable + data pointer; no allocation, no syscall. The clone
    /// exists so the producer can release the buffer's mutex *before*
    /// firing the wake, without losing the right to wake.
    fn take_waker_clone(&self) -> Option<Waker>;
}

#[derive(Clone)]
pub(crate) struct ConnectionManagedBuffers {
    pub(crate) conn_buff: Arc<Mutex<ConnectionBuffer>>,
    pub(crate) ack_buff: Arc<Mutex<AckBuffer>>,
    pub(crate) diag_tx: Option<DiagSender>,
}

/// Helper to build `num_tx_workers` number of tx workers to run.
#[inline]
fn build_and_start_tx(
    num_tx_workers: u16,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<ConnectionManager>,
    hello_state: Arc<Mutex<HelloState>>,
    host_type: BluefinHost,
) {
    let tx = ReaderTxChannel::new(socket, conn_manager, hello_state, host_type);

    for id in 0..num_tx_workers {
        let mut tx_clone = tx.clone();
        tx_clone.id = id;
        spawn(async move {
            let _ = tx_clone.run().await;
        });
    }
}

#[inline]
fn build_and_start_conn_reader_tx_channels(
    socket: Arc<UdpSocket>,
    conn_bufs: Arc<ConnectionManagedBuffers>,
) -> BluefinResult<()> {
    let handler = ConnReaderHandler::new(socket, conn_bufs);
    handler.start()
}

#[inline]
fn build_and_start_ack_consumer_workers(
    num_ack_consumer_workers: u8,
    ack_buffer: Arc<Mutex<AckBuffer>>,
) {
    let largest_recv_acked_packet_num = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ack_consumer = AckConsumer::new(Arc::clone(&ack_buffer), largest_recv_acked_packet_num);

    for _ in 0..num_ack_consumer_workers {
        let ack_consumer_clone = ack_consumer.clone();
        spawn(async move {
            ack_consumer_clone.run().await;
        });
    }
}

/// Helper to determine whether a given `packet` is a valid hello packet eg. client-hello or pack-leader-hello
#[inline]
pub(crate) fn is_hello_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    // For a server, the handshake must be initiated by an client hello
    if host_type == BluefinHost::PackLeader
        && packet.header.type_field != PacketType::UnencryptedClientHello
    {
        return false;
    }

    // For a client, the handshake must be followed up by an server hello
    if host_type == BluefinHost::Client
        && packet.header.type_field != PacketType::UnencryptedServerHello
    {
        return false;
    }

    // For a client receiving a server hello, both ids MUST be set
    if host_type == BluefinHost::Client && (other_id == 0x0 || this_id == 0x0) {
        return false;
    }

    // if handshake, must have a non-zero source id
    if host_type == BluefinHost::PackLeader && other_id == 0x0 {
        return false;
    }

    // if handshake, the destination id must be 0x0
    if host_type == BluefinHost::PackLeader && this_id != 0x0 {
        return false;
    }

    true
}

#[inline]
pub(crate) fn is_client_ack_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    if host_type == BluefinHost::PackLeader
        && packet.header.type_field == PacketType::ClientAck
        && other_id != 0x0
        && this_id != 0x0
    {
        return true;
    }
    false
}

#[inline]
pub(crate) fn build_empty_encrypted_packet(
    src_conn_id: u32,
    dst_conn_id: u32,
    packet_number: u64,
    packet_type: PacketType,
) -> BluefinPacket {
    let security_fields = BluefinSecurityFields::new(false, 0x0);
    let mut header =
        BluefinHeader::new(src_conn_id, dst_conn_id, packet_type, 0x0, security_fields);
    header.with_packet_number(packet_number);
    BluefinPacket::builder().header(header).build()
}
