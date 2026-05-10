use tokio::net::UdpSocket;
use tokio::spawn;
use tokio::sync::mpsc::{self};

use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::core::Extract;
use crate::net::ack_handler::AckBuffer;
use crate::net::connection::ConnectionBuffer;
use crate::net::{ConnectionManagedBuffers, Wakeable, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::BytesMut;
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

        loop {
            // One heap allocation per recv. We trade up to ~10 small
            // per-payload `Vec::with_capacity` allocations inside
            // `from_bytes_into` for a single 15 KiB `BytesMut` here. Every
            // packet payload sliced out of `frozen` below is a refcount
            // bump on this same allocation, so the buffer lives exactly as
            // long as the longest-held payload.
            let mut buf = BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

            // Hand recv a `&mut [u8]` over the spare capacity. We use
            // `set_len(MAX)` rather than `BytesMut::zeroed(MAX)` to skip
            // the 15 KiB memset on the hot path (mirrors the previous
            // `MaybeUninit<[u8; MAX]>` idiom). The bytes are formally
            // uninit until recv writes them; we never read past `size`.
            //
            // SAFETY: `recv` writes `size` bytes into the buffer before
            // returning; `truncate(size)` immediately drops the
            // still-uninit tail so no consumer can ever observe it via the
            // `Bytes` API.
            unsafe { buf.set_len(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM); }
            let size = socket.recv(&mut buf[..]).await?;
            buf.truncate(size);

            // `freeze()` converts the BytesMut into an immutable Bytes that
            // can be cheaply sliced into refcount views — one per parsed
            // packet payload.
            let frozen = buf.freeze();
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
    /// Two reverted experiments are documented in the SKILL file:
    /// - Round J (recvmsg_x reader on its own): regressed delivered
    ///   throughput by 9 % because the writer rarely produced multi-datagram
    ///   bursts, so each `recvmsg_x` returned 1 datagram and paid the
    ///   per-call setup overhead for nothing.
    /// - Round K (paired sendmsg_x writer + recvmsg_x reader, with
    ///   `tokio::task::yield_now()` pacing): healthy peak +9 % but
    ///   bilateral reliability dropped to 5/10 even with `kern.ipc.maxsockbuf`
    ///   bumped to 32 MB. `yield_now` is a no-op when no other task is
    ///   queued, so the writer outpaced the reader.
    ///
    /// `macos_io::recvmsg_x_into` is preserved for a future round that
    /// pairs with proper application-level pacing.
    #[inline]
    async fn recv_and_buffer_inline(
        socket: Arc<UdpSocket>,
        conn_bufs: Arc<ConnectionManagedBuffers>,
    ) -> BluefinResult<()> {
        const PACKETS_VEC_CAPACITY: usize = 76;
        let mut packets: Vec<BluefinPacket> = Vec::with_capacity(PACKETS_VEC_CAPACITY);

        loop {
            // One heap allocation per recv (see `tx_impl` for the rationale).
            let mut buf = BytesMut::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
            // SAFETY: `recv` writes `size` bytes into the buffer before
            // returning; `truncate(size)` immediately drops the still-uninit
            // tail.
            unsafe { buf.set_len(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM); }
            let size = socket.recv(&mut buf[..]).await?;
            buf.truncate(size);
            let frozen = buf.freeze();

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
                Self::buffer_in_ack_packets(guard, packets)
            }
            _ => {
                let guard = conn_bufs.conn_buff.lock().unwrap();
                Self::buffer_in_data_packets(guard, packets)
            }
        }
    }

    #[inline]
    fn buffer_in_ack_packets(
        mut guard: MutexGuard<'_, AckBuffer>,
        packets: &mut Vec<BluefinPacket>,
    ) -> BluefinResult<()> {
        let mut e: Option<BluefinError> = None;
        // `drain(..)` empties the vec but keeps its capacity, so the caller's
        // carrier vec is reusable for the next datagram without realloc.
        for p in packets.drain(..) {
            if let Err(err) = guard.buffer_in_ack_packet(p) {
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
