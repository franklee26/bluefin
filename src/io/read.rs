use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use etherparse::PacketHeaders;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, ReadBuf},
};

use crate::{
    core::{header::PacketType, packet::BluefinPacket, serialisable::Serialisable},
    network::connection::Connection,
};

use super::manager::{ConnectionManager, Result};

pub struct Accept<'a> {
    pub(crate) file: File,
    pub(crate) conn_manager: &'a mut ConnectionManager,
    pub(crate) need_ip_and_udp_headers: bool,
}

impl<'a> Accept<'a> {
    pub(crate) fn new(
        mut file: File,
        conn_manager: &'a mut ConnectionManager,
        need_ip_and_udp_headers: bool,
    ) -> Self {
        Self {
            file,
            conn_manager,
            need_ip_and_udp_headers,
        }
    }

    /// Reads in from the wire an incoming stream of bytes. We try to deserialise the bytes into an
    /// bluefin packet.
    fn poll_read_bluefin_packet(&mut self, cx: &mut Context) -> Result<BluefinPacket> {
        let pinned_file = Pin::new(&mut self.file);
        let mut temp_buf = vec![0; 1504];
        let mut buf = ReadBuf::new(&mut temp_buf);
        // TODO: fix unwrap
        let _ = pinned_file.poll_read(cx, &mut buf);
        let mut offset = 0;

        let read_buf = buf.filled();

        // I need to strip the ip and udp headers
        if self.need_ip_and_udp_headers {
            let ip_packet = PacketHeaders::from_ip_slice(&read_buf[4..]).unwrap();
            let ip_header_len = ip_packet.ip.unwrap().header_len();
            let udp_header_len = ip_packet.transport.unwrap().udp().unwrap().header_len();
            offset = 4 + ip_header_len + udp_header_len;
        }

        let packet = BluefinPacket::deserialise(&read_buf[offset..])?;

        Ok(packet)
    }
}

impl Future for Accept<'_> {
    type Output = Connection;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _self = self.get_mut();

        match _self.poll_read_bluefin_packet(cx) {
            Ok(packet) => {
                let other_id = packet.header.source_connection_id;
                let this_id = packet.header.destination_connection_id;
                let key = format!("{}_{}", other_id, this_id);

                // This already exists... let's redirect it to the correct buffer and wait...
                if _self.conn_manager.conn_exists(&key) {
                    _self.conn_manager.add_buffer_to_connection(&key, packet);
                    // Register new waker
                    _self
                        .conn_manager
                        .add_new_connection_waker(cx.waker().clone());
                    return Poll::Pending;
                }

                // Packet describes a new connection but is not a handshake request... disregard
                if packet.header.type_and_type_specific_payload.packet_type
                    != PacketType::UnencryptedHandshake
                {
                    // Register new waker
                    _self
                        .conn_manager
                        .add_new_connection_waker(cx.waker().clone());
                    return Poll::Pending;
                }

                Poll::Pending
            }
            Err(_) => Poll::Pending,
        }
    }
}
