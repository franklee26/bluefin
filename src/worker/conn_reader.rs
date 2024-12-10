use tokio::net::UdpSocket;
use tokio::task::yield_now;
use tokio::time::{sleep, Sleep};

use crate::core::error::BluefinError;
use crate::core::header::PacketType;
use crate::core::packet::BluefinPacket;
use crate::net::{ConnectionManagedBuffers, MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM};
use crate::utils::common::BluefinResult;
use crate::utils::get_connected_udp_socket;
use std::mem;
use std::os::fd::AsRawFd;
use std::time::Duration;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub(crate) struct ConnReaderTxChannel {
    id: u16,
    socket: Arc<UdpSocket>,
    conn_bufs: Arc<ConnectionManagedBuffers>,
}

impl ConnReaderTxChannel {
    pub(crate) fn new(
        id: u16,
        socket: Arc<UdpSocket>,
        conn_bufs: Arc<ConnectionManagedBuffers>,
    ) -> Self {
        Self {
            id,
            socket,
            conn_bufs,
        }
    }

    pub(crate) async fn run(&self) -> BluefinResult<()> {
        let mut buf = [0u8; MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM];
        loop {
            if let Err(e) = self.run_impl(&mut buf).await {
                eprintln!("Encountered err in conn_reader: {:?}", e);
            }
        }
    }

    #[inline]
    async fn run_impl(&self, buf: &mut [u8]) -> BluefinResult<()> {
        let size = self.socket.recv(buf).await?;
        let packets = BluefinPacket::from_bytes(&buf[..size])?;
        // eprintln!("Active {}, {} packet(s)", self.id, packets.len());

        if packets.len() == 0 {
            return Err(BluefinError::Unexpected(
                "UDP payload contains zero bluefin packets".to_string(),
            ));
        }

        let mut err = None;
        for p in packets {
            if let Err(e) = self.buffer_in_packet(p).await {
                err = Some(e);
            }
        }

        if err.is_some() {
            return Err(err.unwrap());
        }

        Ok(())
    }

    async fn buffer_in_packet(&self, packet: BluefinPacket) -> BluefinResult<()> {
        if packet.header.type_field == PacketType::Ack {
            let mut guard = self.conn_bufs.ack_buff.lock().unwrap();
            guard.buffer_in_ack_packet(packet)?;
            guard.wake()?;
        } else {
            let mut guard = self.conn_bufs.conn_buff.lock().unwrap();
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
