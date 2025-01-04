use tokio::net::UdpSocket;
use tokio::spawn;
use tokio::sync::mpsc::{self};

use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::core::Extract;
use crate::net::ack_handler::AckBuffer;
use crate::net::connection::ConnectionBuffer;
use crate::net::{ConnectionManagedBuffers, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
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
    pub(crate) fn start(&self) -> BluefinResult<()> {
        let (tx, rx) = mpsc::channel::<Vec<BluefinPacket>>(1024);

        // Spawn n-number of UDP-recv tasks.
        for _ in 0..Self::get_number_of_tx_tasks() {
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
        let mut buf = [0u8; MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM];
        loop {
            let size = socket.recv(&mut buf).await?;
            let packets = BluefinPacket::from_bytes(&buf[..size])?;

            let _ = tx.send(packets).await;
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
            if let Some(packets) = rx.recv().await {
                let _ = Self::buffer_in_packets(packets, conn_bufs);
            }
        }
    }

    #[inline]
    fn buffer_in_packets(
        packets: Vec<BluefinPacket>,
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
        let p = packets.first().unwrap();
        match p.header.type_field {
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
        packets: Vec<BluefinPacket>,
    ) -> BluefinResult<()> {
        let mut e: Option<BluefinError> = None;
        for p in packets {
            if let Err(err) = guard.buffer_in_ack_packet(p) {
                e = Some(err);
            }
        }
        guard.wake()?;

        if e.is_some() {
            return Err(e.unwrap());
        }
        Ok(())
    }

    #[inline]
    fn buffer_in_data_packets(
        mut guard: MutexGuard<'_, ConnectionBuffer>,
        packets: Vec<BluefinPacket>,
    ) -> BluefinResult<()> {
        let mut e: Option<BluefinError> = None;
        for mut p in packets {
            if let Err(err) = guard.buffer_in_bytes(p.extract()) {
                e = Some(err);
            }
        }

        if let Some(w) = guard.get_waker() {
            w.wake_by_ref();
        } else {
            return Err(BluefinError::NoSuchWakerError);
        }

        if e.is_some() {
            return Err(e.unwrap());
        }
        Ok(())
    }
}
