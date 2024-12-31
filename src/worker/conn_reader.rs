use tokio::net::UdpSocket;
use tokio::spawn;
use tokio::sync::mpsc::{self};

use crate::core::error::BluefinError;
use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::net::ack_handler::AckBuffer;
use crate::net::connection::ConnectionBuffer;
use crate::net::{ConnectionManagedBuffers, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM};
use crate::utils::common::BluefinResult;
use std::sync::{Arc, MutexGuard};
use std::thread::available_parallelism;

pub(crate) struct ConnReaderHandler {
    socket: Arc<UdpSocket>,
    conn_bufs: Arc<ConnectionManagedBuffers>,
}

impl ConnReaderHandler {
    pub(crate) fn new(socket: Arc<UdpSocket>, conn_bufs: Arc<ConnectionManagedBuffers>) -> Self {
        Self { socket, conn_bufs }
    }

    pub(crate) fn start(&self) -> BluefinResult<()> {
        let (tx, rx) = mpsc::channel::<Vec<BluefinPacket>>(1024);
        let num_cpu_cores = available_parallelism()?.get();
        for _ in 0..1 {
            let tx_cloned = tx.clone();
            let socket_cloned = self.socket.clone();
            spawn(async move {
                let _ = ConnReaderHandler::tx_impl(socket_cloned, tx_cloned).await;
            });
        }

        let conn_bufs = self.conn_bufs.clone();
        spawn(async move {
            let _ = ConnReaderHandler::rx_impl(rx, &*conn_bufs).await;
        });
        Ok(())
    }

    #[inline]
    async fn tx_impl(
        socket: Arc<UdpSocket>,
        tx: mpsc::Sender<Vec<BluefinPacket>>,
    ) -> BluefinResult<()> {
        let mut buf = [0u8; MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM];
        loop {
            let size = socket.recv(&mut buf).await?;
            let packets = BluefinPacket::from_bytes(&buf[..size])?;

            if packets.len() == 0 {
                continue;
            }

            let _ = tx.send(packets).await;
        }
    }

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

        // Peek at the first packet and acquire the buffer.
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
        for p in packets {
            if let Err(err) = guard.buffer_in_bytes(p) {
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
