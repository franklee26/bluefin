use tokio::net::UdpSocket;
use tokio::spawn;
use tokio::sync::mpsc::{self};

use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::core::Extract;
use crate::net::ack_handler::AckBuffer;
use crate::net::connection::ConnectionBuffer;
use crate::net::{
    diag_try_send, ConnectionManagedBuffers, DiagSender, DiagnosticEvent, Wakeable,
    MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM,
};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::{Bytes, BytesMut};
use std::sync::{Arc, MutexGuard};

/// This is arbitrary number of worker tasks to use if we cannot decide how many worker tasks
/// to spawn.
const DEFAULT_NUMBER_OF_TASKS_TO_SPAWN: usize = 3;

/// [ConnReaderHandler] is a handle to network read-related functionalities. As the name suggests,
/// we this handler is specific for *connection* reads. That is, this handler can only be used
/// when a Bluefin connection has been established. This reader is fundamentally different from that
/// of the [crate::worker::reader::ReaderRxChannel] as this will only read packets from the wire
/// intended for the connection.
pub(crate) struct ConnReaderHandler {
    socket: Arc<UdpSocket>,
    conn_bufs: Arc<ConnectionManagedBuffers>,
}

impl ConnReaderHandler {
    pub(crate) fn new(socket: Arc<UdpSocket>, conn_bufs: Arc<ConnectionManagedBuffers>) -> Self {
        Self { socket, conn_bufs }
    }

    /// Starts the handler worker jobs. This starts the worker tasks, which busy-polls a connected
    /// UDP socket for packets. Upon receiving bytes, these workers will send them to another
    /// channel for processing. Then second kind of worker is the processing channel, which receives
    /// bytes, attempts to deserialise them into bluefin packets and buffer them in the correct
    /// buffer.
    ///
    /// On platforms where [`Self::get_number_of_tx_tasks`] returns 1 (macOS —
    /// `SO_REUSEPORT` doesn't fan packets across sockets the way it does on
    /// Linux), we collapse the two-task tx → mpsc → rx pipeline into a single
    /// task that recvs and buffers in-line. That removes one waker hop and
    /// one channel send per recv, and keeps the parsed-packet carrier vec
    /// alive across iterations so we don't alloc a fresh `Vec<BluefinPacket>`
    /// for every datagram. Linux still uses the multi-producer mpsc shape so
    /// N parallel recv tasks can fan into one buffer task.
    pub(crate) fn start(&self) -> BluefinResult<()> {
        let n = Self::get_number_of_tx_tasks();

        if n == 1 {
            let socket = self.socket.clone();
            let conn_bufs = self.conn_bufs.clone();
            spawn(async move {
                let _ = ConnReaderHandler::recv_and_buffer_inline(socket, conn_bufs).await;
            });
            return Ok(());
        }

        let (tx, rx) = mpsc::channel::<Vec<BluefinPacket>>(1024);

        // Spawn n-number of UDP-recv tasks.
        for _ in 0..n {
            let tx_cloned = tx.clone();
            let socket_cloned = self.socket.clone();
            spawn(async move {
                let _ = ConnReaderHandler::tx_impl(socket_cloned, tx_cloned).await;
            });
        }

        // Spawn the corresponding rx channel which receives bytes from the tx channel and processes
        // bytes and buffers them.
        let conn_bufs = self.conn_bufs.clone();
        spawn(async move {
            let _ = ConnReaderHandler::rx_impl(rx, &*conn_bufs).await;
        });
        Ok(())
    }

    /// For linux, we return the expected number of CPU cores. This lets us take advantage of
    /// parallelism. For (silicon) macos, we return one. Experiments on Apple Silicon have shown
    /// that SO_REUSEPORT does not behave the same way as it does on Linux
    /// (see: https://stackoverflow.com/questions/51998042/macos-so-reuseaddr-so-reuseport-not-consistent-with-linux)
    /// and so we cannot take advantage of running the rx-tasks on multiple threads. For now, running
    /// one instance of it is performant enough.
    ///
    /// For all other operating systems (which is currently unsupported by Bluefine anyways), we
    /// return an arbitrary default value.
    #[allow(unreachable_code)]
    #[inline]
    fn get_number_of_tx_tasks() -> usize {
        // For linux, let's use all the cpu cores available.
        #[cfg(target_os = "linux")]
        {
            use std::thread::available_parallelism;
            if let Ok(num_cpu_cores) = available_parallelism() {
                return num_cpu_cores.get();
            }
        }

        // For macos (at least silicon macs), we can't seem to use
        // SO_REUSEPORT to our benefit. We will pretend we have one core.
        #[cfg(target_os = "macos")]
        {
            return 1;
        }

        // For everything else, we assume the default.
        DEFAULT_NUMBER_OF_TASKS_TO_SPAWN
    }

    /// This represents one tx task or one of the multiple producers in the mpsc channel. This
    /// function is a hot-loop; it continuously reads from a connected socket. When bytes are
    /// received, we attempt to deserialise them into bluefin packets. If valid packets are
    /// produced, them we send them to the consumer channel for processing.
    #[inline]
    async fn tx_impl(
        socket: Arc<UdpSocket>,
        tx: mpsc::Sender<Vec<BluefinPacket>>,
    ) -> BluefinResult<()> {
        // Capacity for the parsed-packet carrier. 76 is a safe upper bound for
        // MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM / minimum packet size.
        const PACKETS_VEC_CAPACITY: usize = 76;
        let mut packets: Vec<BluefinPacket> = Vec::with_capacity(PACKETS_VEC_CAPACITY);
        // 1-slot recv-buffer recycle. We hold one extra refcount on the
        // just-frozen `Bytes` across one iteration; if all per-packet slices
        // (and the consumer's downstream Bytes views) have been dropped by
        // the time we loop back, `try_into_mut` returns the same 15 KiB
        // allocation and we save a malloc/free pair. If the consumer is slow,
        // `try_into_mut` returns `Err(b)`, we drop our refcount, and alloc
        // fresh — no harm, just a missed opportunity. Bounded memory growth
        // (one extra buffer-lifetime of headroom per task).
        let mut recycle: Option<Bytes> = None;

        loop {
            // Acquire a recv buffer: prefer recycling, fall back to alloc.
            let mut buf = match recycle.take() {
                Some(b) => b
                    .try_into_mut()
                    .unwrap_or_else(|_| BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM)),
                None => BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM),
            };
            debug_assert!(buf.capacity() >= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

            // Hand recv a `&mut [u8]` over the spare capacity. We use
            // `set_len(MAX)` rather than `BytesMut::zeroed(MAX)` to skip
            // the 15 KiB memset on the hot path (mirrors the previous
            // `MaybeUninit<[u8; MAX]>` idiom). The bytes are formally
            // uninit until recv writes them; we never read past `size`.
            //
            // SAFETY: `recv` writes `size` bytes into the buffer before
            // returning; `truncate(size)` immediately drops the
            // still-uninit tail so no consumer can ever observe it via the
            // `Bytes` API. On the recycled path the bytes were previously
            // initialised by the prior recv — still sound for `set_len`.
            unsafe { buf.set_len(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM); }
            let size = socket.recv(&mut buf[..]).await?;
            buf.truncate(size);

            // `freeze()` converts the BytesMut into an immutable Bytes that
            // can be cheaply sliced into refcount views — one per parsed
            // packet payload.
            let frozen = buf.freeze();
            // Stash one refcount for next iteration's recycle attempt.
            // Cheap (atomic increment); the buffer is recycled the moment
            // every other holder has dropped.
            recycle = Some(frozen.clone());
            BluefinPacket::from_bytes_into(frozen, &mut packets)?;

            // Hand the populated vec to the consumer without cloning the
            // payloads. We then re-create an empty vec for the next iteration;
            // this is one small allocation per datagram instead of cloning
            // every parsed packet's payload (each up to ~1500 B).
            //
            // `mem::take` leaves `packets` as an empty Vec with no capacity,
            // so the subsequent `with_capacity` is necessary to keep the
            // amortised behaviour we had before.
            let to_send = std::mem::take(&mut packets);
            packets = Vec::with_capacity(PACKETS_VEC_CAPACITY);
            let _ = tx.send(to_send).await;
        }
    }

    /// This is the single consumer in the mpsc channel. This receives bluefin packets from
    /// n-producers. We place the packets into the relevant buffer.
    #[inline]
    async fn rx_impl(
        mut rx: mpsc::Receiver<Vec<BluefinPacket>>,
        conn_bufs: &ConnectionManagedBuffers,
    ) {
        loop {
            if let Some(mut packets) = rx.recv().await {
                let _ = Self::buffer_in_packets(&mut packets, conn_bufs);
            }
        }
    }

    /// Single-task hot loop: recv, parse, and buffer in one go. Used when
    /// only one tx task would have been spawned (macOS) so the mpsc channel
    /// + dedicated buffer task were pure overhead. Saves one waker hop per
    /// recv and reuses the parsed-packet carrier vec across iterations.
    ///
    /// Vectorised I/O (recvmsg_x batches) was tried and reverted: under the
    /// current writer, multi-datagram bursts are rare so each batch returns
    /// 1 datagram and pays the per-call setup overhead for nothing. The
    /// `macos_io::recvmsg_x_into` binding remains in tree for a future round
    /// that pairs it with a paced batched writer.
    #[inline]
    async fn recv_and_buffer_inline(
        socket: Arc<UdpSocket>,
        conn_bufs: Arc<ConnectionManagedBuffers>,
    ) -> BluefinResult<()> {
        const PACKETS_VEC_CAPACITY: usize = 76;
        let mut packets: Vec<BluefinPacket> = Vec::with_capacity(PACKETS_VEC_CAPACITY);
        // 1-slot recv-buffer recycle (see `tx_impl` for full rationale).
        let mut recycle: Option<Bytes> = None;

        loop {
            // Recycle the prior buffer if all slices have dropped, else alloc.
            let mut buf = match recycle.take() {
                Some(b) => b
                    .try_into_mut()
                    .unwrap_or_else(|_| BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM)),
                None => BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM),
            };
            debug_assert!(buf.capacity() >= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
            // SAFETY: `recv` writes `size` bytes into the buffer before
            // returning; `truncate(size)` immediately drops the still-uninit
            // tail. On the recycled path the bytes were init'd by the prior
            // recv — still sound for `set_len`.
            unsafe { buf.set_len(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM); }
            let size = socket.recv(&mut buf[..]).await?;
            buf.truncate(size);
            let frozen = buf.freeze();
            // Stash one refcount for next iteration's recycle attempt.
            recycle = Some(frozen.clone());

            // Reuse the carrier vec; `from_bytes_into` only ever appends.
            packets.clear();
            if BluefinPacket::from_bytes_into(frozen, &mut packets).is_err() {
                continue;
            }

            // Buffer in-place via `&mut packets` so the Vec keeps its
            // capacity for the next iteration. No mpsc, no waker hop.
            let _ = Self::buffer_in_packets(&mut packets, &conn_bufs);
        }
    }

    #[inline]
    fn buffer_in_packets(
        packets: &mut Vec<BluefinPacket>,
        conn_bufs: &ConnectionManagedBuffers,
    ) -> BluefinResult<()> {
        // Nothing to do if empty
        if packets.is_empty() {
            return Ok(());
        }

        // Peek at the first packet and acquire the buffer. The assumptions here are:
        // 1. An udp datagram contains one or more bluefin packets. However, all the packets
        //    in the datagram are for the same connection (no mix and matching different connection
        //    packets in the same datagram).
        // 2. An udp datagram contains the same type of packets. This means a udp datagram either
        //    contains all data-type packets or ack-packets.
        // Therefore, with these assumptions, we can just peek at the first packet in the datagram
        // and then acquire the appropriate lock before processing.
        let first_type = packets.first().unwrap().header.type_field;
        match first_type {
            PacketType::Ack => {
                let guard = conn_bufs.ack_buff.lock().unwrap();
                Self::buffer_in_ack_packets(guard, packets, &conn_bufs.diag_tx)
            }
            PacketType::Fin | PacketType::FinAck => {
                Self::buffer_in_close_packets(packets, conn_bufs)
            }
            _ => {
                let guard = conn_bufs.conn_buff.lock().unwrap();
                Self::buffer_in_data_packets(guard, packets)
            }
        }
    }

    /// Routes `Fin` / `FinAck` packets into the per-connection
    /// [`crate::net::close_handler::CloseBuffer`] and, on `Fin`, wakes any
    /// task currently parked in [`crate::worker::reader::ReaderRxChannel`]
    /// so that it can surface EOF to the application.
    ///
    /// Per spec \u00a78, all packets in a single UDP datagram are the same
    /// class. A datagram whose first packet is a `Fin`/`FinAck` is therefore
    /// expected to contain a single such packet (\u00a710bis: own datagram).
    /// We tolerate a hypothetical batched form by walking the vec.
    #[inline]
    fn buffer_in_close_packets(
        packets: &mut Vec<BluefinPacket>,
        conn_bufs: &ConnectionManagedBuffers,
    ) -> BluefinResult<()> {
        let mut saw_fin = false;
        let mut pending_fin_ack_pn: Option<u64> = None;
        {
            let mut guard = conn_bufs.close_buff.lock().unwrap();
            for p in packets.drain(..) {
                match p.header.type_field {
                    PacketType::Fin => {
                        diag_try_send(
                            &conn_bufs.diag_tx,
                            DiagnosticEvent::FinReceived {
                                packet_num: p.header.packet_number,
                            },
                        );
                        guard.buffer_in_fin(&p);
                        saw_fin = true;
                    }
                    PacketType::FinAck => {
                        diag_try_send(
                            &conn_bufs.diag_tx,
                            DiagnosticEvent::FinAckReceived {
                                packet_num: p.header.packet_number,
                            },
                        );
                        guard.buffer_in_fin_ack(&p);
                    }
                    _ => unreachable!(
                        "buffer_in_close_packets only routes Fin/FinAck"
                    ),
                }
            }
            // Drain at most one FinAck-send obligation per call. If we
            // received the peer's FIN, take the pn we owe back.
            if saw_fin {
                pending_fin_ack_pn = guard.take_pending_fin_ack_send();
            }
            // Drop the close-buffer guard before touching the data buffer
            // to keep the locks strictly nested in one direction.
        }

        // Fire the FinAck send (if any). Best-effort try_send: if the
        // bounded channel is full, the in-flight drainer will catch up
        // shortly and the next duplicate FIN from the peer (if needed)
        // will re-arm `pending_fin_ack_send`.
        if let (Some(pn), Some(tx)) = (pending_fin_ack_pn, conn_bufs.fin_ack_tx.as_ref()) {
            let _ = tx.try_send(pn);
        }

        if saw_fin {
            // Wake the data buffer's waker so a pending recv() observes
            // `peer_fin_observed == true` and returns EOF. Cloning the
            // waker out under the data-buffer mutex, then dropping the
            // guard before waking, follows the same producer pattern as
            // `buffer_in_data_packets` (see bluefin-architecture \u00a75).
            let mut guard = conn_bufs.conn_buff.lock().unwrap();
            let waker = guard.take_waker_clone();
            drop(guard);
            if let Some(w) = waker {
                w.wake();
            }
        }
        Ok(())
    }

    #[inline]
    fn buffer_in_ack_packets(
        mut guard: MutexGuard<'_, AckBuffer>,
        packets: &mut Vec<BluefinPacket>,
        diag_tx: &Option<DiagSender>,
    ) -> BluefinResult<()> {
        let mut e: Option<BluefinError> = None;
        // `drain(..)` empties the vec but keeps its capacity, so the caller's
        // carrier vec is reusable for the next datagram without realloc.
        for p in packets.drain(..) {
            let base = p.header.packet_number;
            let count = p.header.type_specific_payload as usize;
            if let Err(err) = guard.buffer_in_ack_packet(p) {
                e = Some(err);
            } else {
                diag_try_send(
                    diag_tx,
                    DiagnosticEvent::AckReceived {
                        base_packet_num: base,
                        count,
                    },
                );
            }
        }
        // Clone the waker out (cheap atomic refcount), then drop the guard
        // BEFORE waking. Waking while still holding the lock causes the woken
        // task to immediately bounce on `lock()`.
        let waker = guard.take_waker_clone();
        drop(guard);

        match waker {
            Some(w) => w.wake(),
            None => return Err(BluefinError::NoSuchWakerError),
        }

        if e.is_some() {
            return Err(e.unwrap());
        }
        Ok(())
    }

    #[inline]
    fn buffer_in_data_packets(
        mut guard: MutexGuard<'_, ConnectionBuffer>,
        packets: &mut Vec<BluefinPacket>,
    ) -> BluefinResult<()> {
        let mut e: Option<BluefinError> = None;
        for mut p in packets.drain(..) {
            if let Err(err) = guard.buffer_in_bytes(p.extract()) {
                e = Some(err);
            }
        }

        // Clone the waker out (cheap atomic refcount), then drop the guard
        // BEFORE waking. Waking while still holding the lock causes the woken
        // task to immediately bounce on `lock()`.
        let waker = guard.take_waker_clone();
        drop(guard);

        match waker {
            Some(w) => w.wake(),
            None => return Err(BluefinError::NoSuchWakerError),
        }

        if e.is_some() {
            return Err(e.unwrap());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::header::{BluefinHeader, BluefinSecurityFields, PacketType};
    use crate::core::packet::BluefinPacket;
    use crate::net::ack_handler::AckBuffer;
    use crate::net::close_handler::{CloseBuffer, CloseState};
    use crate::net::connection::ConnectionBuffer;
    use bluefin_proto::context::BluefinHost;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    /// Test waker that records whether it was invoked.
    struct CountingWaker {
        woken: AtomicBool,
    }

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.woken.store(true, Ordering::Release);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.woken.store(true, Ordering::Release);
        }
    }

    fn fin_packet(src: u32, dst: u32, pn: u64) -> BluefinPacket {
        let mut header = BluefinHeader::new(
            src,
            dst,
            PacketType::Fin,
            0x0,
            BluefinSecurityFields::default(),
        );
        header.with_packet_number(pn);
        BluefinPacket::builder().header(header).build()
    }

    fn fin_ack_packet(src: u32, dst: u32, pn: u64) -> BluefinPacket {
        let mut header = BluefinHeader::new(
            src,
            dst,
            PacketType::FinAck,
            0x0,
            BluefinSecurityFields::default(),
        );
        header.with_packet_number(pn);
        BluefinPacket::builder().header(header).build()
    }

    fn make_managed_bufs() -> (ConnectionManagedBuffers, Arc<AtomicBool>) {
        let conn_buff = Arc::new(Mutex::new(ConnectionBuffer::new(
            0xaaaa_bbbb,
            BluefinHost::Client,
        )));
        // Set an addr so callers that need it (not us, but the recv path
        // does) don't trip up. Buffer-in is harmless here.
        let _ = conn_buff
            .lock()
            .unwrap()
            .buffer_in_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1));
        let ack_buff = Arc::new(Mutex::new(AckBuffer::new(0)));
        let peer_fin_observed = Arc::new(AtomicBool::new(false));
        let close_buff = Arc::new(Mutex::new(CloseBuffer::new(Arc::clone(
            &peer_fin_observed,
        ))));
        let bufs = ConnectionManagedBuffers {
            conn_buff,
            ack_buff,
            close_buff,
            peer_fin_observed: Arc::clone(&peer_fin_observed),
            fin_ack_tx: None,
            diag_tx: None,
        };
        (bufs, peer_fin_observed)
    }

    #[test]
    fn buffer_in_packets_routes_fin_to_close_buffer_and_wakes_data_waker() {
        let (bufs, peer_fin_observed) = make_managed_bufs();

        // Register a waker on the conn_buff so we can assert it was woken
        // by the close-handler code path.
        let counter = Arc::new(CountingWaker {
            woken: AtomicBool::new(false),
        });
        let waker: Waker = counter.clone().into();
        {
            let mut g = bufs.conn_buff.lock().unwrap();
            g.set_waker_if_changed(&waker);
        }
        // Touch the cx parameter shape to ensure the waker round-trips
        // (smoke check, no real polling here).
        let _cx = Context::from_waker(&waker);

        let mut packets = vec![fin_packet(0x1, 0x2, 9_999)];
        let res = ConnReaderHandler::buffer_in_packets(&mut packets, &bufs);
        assert!(res.is_ok());

        // Close buffer transitioned and lock-free flag is set.
        assert!(peer_fin_observed.load(Ordering::Acquire));
        let close_state = bufs.close_buff.lock().unwrap().state();
        assert_eq!(
            close_state,
            CloseState::PeerFinReceived {
                fin_packet_num: 9_999,
            }
        );
        // Data buffer's waker was invoked so a parked recv() returns EOF.
        assert!(counter.woken.load(Ordering::Acquire));
        // Vec drained.
        assert!(packets.is_empty());
    }

    #[test]
    fn buffer_in_packets_routes_fin_ack_to_close_buffer_without_setting_peer_fin_flag() {
        let (bufs, peer_fin_observed) = make_managed_bufs();

        let mut packets = vec![fin_ack_packet(0x1, 0x2, 1234)];
        ConnReaderHandler::buffer_in_packets(&mut packets, &bufs).unwrap();

        // FinAck does NOT mean we received a peer FIN.
        assert!(!peer_fin_observed.load(Ordering::Acquire));
        let cb = bufs.close_buff.lock().unwrap();
        assert_eq!(cb.received_fin_ack_for(), Some(1234));
        assert_eq!(cb.state(), CloseState::Active);
    }

    #[test]
    fn fin_with_no_data_waker_registered_is_still_ok() {
        let (bufs, peer_fin_observed) = make_managed_bufs();

        let mut packets = vec![fin_packet(0x1, 0x2, 5)];
        // No waker was registered; the close-handler code path should not
        // error in that case (unlike the data/ack paths which return
        // NoSuchWakerError when nobody is parked).
        ConnReaderHandler::buffer_in_packets(&mut packets, &bufs).unwrap();
        assert!(peer_fin_observed.load(Ordering::Acquire));
    }

    #[test]
    fn future_resolves_when_peer_fin_observed_with_empty_buffer() {
        // Construct ReaderRxChannelFuture-shaped state by hand: the future
        // is private to `worker::reader`, so we exercise the same logic
        // here \u2014 lock-free atomic check after empty `peek()`.
        let (bufs, peer_fin_observed) = make_managed_bufs();

        // Initially, peer_fin_observed = false; no data; `peek` returns
        // BufferEmpty. A real future would return Pending.
        {
            let g = bufs.conn_buff.lock().unwrap();
            assert!(g.peek().is_err());
        }
        assert!(!peer_fin_observed.load(Ordering::Acquire));

        // Inject FIN; flag flips to true.
        let mut packets = vec![fin_packet(0x1, 0x2, 1)];
        ConnReaderHandler::buffer_in_packets(&mut packets, &bufs).unwrap();
        assert!(peer_fin_observed.load(Ordering::Acquire));
        // Buffer is still empty; recv path will see (peer_fin_observed && peek().is_err())
        // and return Ok((0, addr)) \u2014 EOF.
        let g = bufs.conn_buff.lock().unwrap();
        assert!(g.peek().is_err());
        assert!(g.addr().is_some());
    }
}
