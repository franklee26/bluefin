use tokio::net::UdpSocket;
use tokio::spawn;
use tokio::sync::mpsc::{self};

use crate::core::error::BluefinError;
use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::net::{ConnectionManagedBuffers, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM};
use crate::utils::common::BluefinResult;
use std::sync::Arc;

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
        // let (tx, rx) = flume::bounded(1024);
        for _ in 0..4 {
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
                for p in packets {
                    let _ = ConnReaderHandler::buffer_in_packet(conn_bufs, p.clone());
                }
            }
        }
    }

    #[inline]
    fn buffer_in_packet(
        conn_bufs: &ConnectionManagedBuffers,
        packet: BluefinPacket,
    ) -> BluefinResult<()> {
        if packet.header.type_field == PacketType::Ack {
            let mut guard = conn_bufs.ack_buff.lock().unwrap();
            guard.buffer_in_ack_packet(packet)?;
            guard.wake()?;
        } else {
            let mut guard = conn_bufs.conn_buff.lock().unwrap();
            guard.buffer_in_bytes(packet)?;
            if let Some(w) = guard.get_waker() {
                w.wake_by_ref();
            } else {
                return Err(BluefinError::NoSuchWakerError);
            }
        }
        Ok(())
    }
}
