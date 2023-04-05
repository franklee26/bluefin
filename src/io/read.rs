use std::{
    future::Future,
    os::fd::FromRawFd,
    pin::Pin,
    task::{Context, Poll},
};

use etherparse::{Ipv4Header, PacketHeaders};
use rand::distributions::{Alphanumeric, DistString};
use tokio::{
    fs::File,
    io::{AsyncRead, ReadBuf},
};

use crate::{
    core::{
        context::BluefinHost,
        header::PacketType,
        packet::{BluefinPacket, Packet},
        serialisable::Serialisable,
    },
    network::connection::Connection,
};

use super::manager::ConnectionManager;

/// Bluefin reader for reading and parsing incoming wire bytes
struct Reader {
    file: File,
    need_ip_and_udp_headers: bool,
}

impl Reader {
    fn new(fd: i32, need_ip_and_udp_headers: bool) -> Self {
        let file = unsafe { File::from_raw_fd(fd) };
        Self {
            file,
            need_ip_and_udp_headers,
        }
    }

    /// Reads in from the wire an incoming stream of bytes. We try to deserialise the bytes
    /// into an bluefin packet but we also keep the ip + udp layers.
    fn poll_read_packet(&self, cx: &mut Context) -> Poll<Packet> {
        let pinned_file = Pin::new(&mut self.file);
        let mut temp_buf = vec![0; 1504];
        let mut buf = ReadBuf::new(&mut temp_buf);
        match pinned_file.poll_read(cx, &mut buf) {
            Poll::Ready(Ok(())) => {
                let read_buf = buf.filled();
                let mut offset = 0;

                // If I need to worry about ip and udp headers then I need to also worry about tun/tap bytes.
                // Offset the tun/tap header, which is 4 bytes
                if self.need_ip_and_udp_headers {
                    offset = 4;
                }

                if let Ok(packet_headers) = PacketHeaders::from_ip_slice(&read_buf[offset..]) {
                    let ip_header_len = packet_headers.ip.unwrap().header_len();
                    let (ip_header, _) =
                        Ipv4Header::from_slice(&read_buf[offset..ip_header_len + offset]).unwrap();

                    // Not udp; drop.
                    if ip_header.protocol != 0x11 {
                        return Poll::Pending;
                    }

                    let udp_header = packet_headers.transport.unwrap().udp().unwrap();
                    let udp_header_len = udp_header.header_len();

                    let src_ip = ip_header.source;
                    let dst_ip = ip_header.destination;
                    let src_port = udp_header.source_port;
                    let dst_port = udp_header.destination_port;

                    let wrapped_bluefin_packet = BluefinPacket::deserialise(
                        &read_buf[offset + ip_header_len + udp_header_len..],
                    );

                    // Not a bluefin packet; drop.
                    if let Err(_) = wrapped_bluefin_packet {
                        return Poll::Pending;
                    }

                    let payload = wrapped_bluefin_packet.unwrap();
                    let packet = Packet {
                        src_ip,
                        dst_ip,
                        src_port,
                        dst_port,
                        payload,
                    };
                    return Poll::Ready(packet);
                }
                Poll::Pending
            }
            _ => Poll::Pending,
        }
    }

    /// The same as `poll_read_packet()` except strip away the ip + udp layer.
    fn poll_read_bluefin_packet(&self, cx: &mut Context) -> Poll<BluefinPacket> {
        match self.poll_read_packet(cx) {
            Poll::Ready(packet) => Poll::Ready(packet.payload),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future representing a new bluefin connection request. This is essentially the same as `Read<'a>` except
/// that it handles the special case of accepting a new connection
pub struct Accept<'device> {
    /// Unique id representing one Accept request. This is required such that we only register at most
    /// one waker per accept request.
    pub id: String,
    pub(crate) fd: i32,
    pub(crate) reader: Reader,
    /// Connection manager is shared across all connections for a device.
    pub(crate) conn_manager: &'device mut ConnectionManager,
    pub(crate) need_ip_and_udp_headers: bool,
}

/// Future for a general read over the network for an existing connection
pub struct Read<'device> {
    /// The id takes the form `<other_id>_<this_id>`. Because a `Read` can only be awaited
    /// on existing connection, then the `other_id` is already defined and must exist in our
    /// manager.
    pub id: String,
    pub(crate) fd: i32,
    pub(crate) reader: Reader,
    /// Connection manager is shared across all connections for a device.
    pub(crate) conn_manager: &'device mut ConnectionManager,
    pub(crate) need_ip_and_udp_headers: bool,
}

impl<'a> Accept<'a> {
    pub(crate) fn new(
        fd: i32,
        conn_manager: &'a mut ConnectionManager,
        need_ip_and_udp_headers: bool,
    ) -> Self {
        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        let reader = Reader::new(fd, need_ip_and_udp_headers);
        Self {
            id,
            fd,
            reader,
            conn_manager,
            need_ip_and_udp_headers,
        }
    }
}

impl Future for Accept<'_> {
    type Output = Connection;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _self = self.get_mut();
        let conn_manager = _self.conn_manager;

        // First check if we managed to buffer the new connection request
        if let Some(conn_buf) = conn_manager.search_new_conn_req_buffer(&_self.id) {
            if let Some(packet) = conn_buf.packet {
                let conn = Connection::new(
                    // The destination id must be whatever the src is calling its src id
                    packet.payload.header.source_connection_id,
                    packet.payload.header.packet_number,
                    packet.src_ip,
                    packet.src_port,
                    packet.dst_ip,
                    packet.dst_port,
                    BluefinHost::PackFollower,
                    _self.need_ip_and_udp_headers,
                    _self.fd,
                );

                // Remove request (it must exist)
                let _ = conn_manager.remove_new_conn_req(_self.id.to_string());

                // Generate new connection key
                let key = format!("{}_{}", conn.dest_id, conn.source_id);
                // Register in map
                conn_manager.register_new_connection(key);

                return Poll::Ready(conn);
            }
        }

        // Didn't find anything in our buffer yet... anything sitting in the socket?
        match _self.reader.poll_read_packet(cx) {
            // Yes! We have a bluefin packet
            Poll::Ready(packet) => {
                let bluefin_packet = packet.payload;
                let other_id = bluefin_packet.header.source_connection_id;
                let this_id = bluefin_packet.header.destination_connection_id;

                let key = format!("{}_{}", other_id, this_id);

                // This already exists... let's redirect it to the correct buffer and wait...
                if _self.conn_manager.conn_exists(&key) {
                    // Add to existing conn's buffer.
                    let _ = conn_manager.buffer_to_existing_connection(&key, packet);
                    // Register this future's waker
                    let _ = conn_manager
                        .register_new_connection_request(_self.id.to_string(), cx.waker().clone());
                    return Poll::Pending;
                }

                // Packet describes a new connection but is not a handshake request... disregard
                if bluefin_packet
                    .header
                    .type_and_type_specific_payload
                    .packet_type
                    != PacketType::UnencryptedHandshake
                {
                    // Register this future's waker
                    let _ = conn_manager
                        .register_new_connection_request(_self.id.to_string(), cx.waker().clone());
                    return Poll::Pending;
                }

                // New connection id AND header is an handshake type BUT the destination id is not
                // set to zero. This is an invalid packet. Disregard.
                if this_id != 0 {
                    // Register this future's waker
                    let _ = conn_manager
                        .register_new_connection_request(_self.id.to_string(), cx.waker().clone());
                    return Poll::Pending;
                }

                // Finally... this is indeed a valid handshake request. Let's create the connection.
                let conn = Connection::new(
                    // The destination id must be whatever the src is calling its src id
                    packet.payload.header.source_connection_id,
                    packet.payload.header.packet_number,
                    packet.src_ip,
                    packet.src_port,
                    packet.dst_ip,
                    packet.dst_port,
                    BluefinHost::PackFollower,
                    _self.need_ip_and_udp_headers,
                    _self.fd,
                );

                // Remove request (if it exists)
                let _ = conn_manager.remove_new_conn_req(key);

                // Generate new connection key
                let key = format!("{}_{}", conn.dest_id, conn.source_id);
                // Register in map
                conn_manager.register_new_connection(key);

                Poll::Ready(conn)
            }
            // Nope, nothing yet... let's put this on pause
            Poll::Pending => {
                // Register this future's waker. It's best to register this since it's possible that
                // another read request would encounter a new-connection request.
                let _ = conn_manager
                    .register_new_connection_request(_self.id.to_string(), cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<'a> Read<'a> {
    pub(crate) fn new(
        id: String,
        fd: i32,
        conn_manager: &'a mut ConnectionManager,
        need_ip_and_udp_headers: bool,
    ) -> Self {
        // Can't have a read request when the connection isn't even registered
        if !conn_manager.conn_exists(&id) {
            panic!("")
        }
        let reader = Reader::new(fd, need_ip_and_udp_headers);
        Self {
            id,
            fd,
            reader,
            conn_manager,
            need_ip_and_udp_headers,
        }
    }
}

impl Future for Read<'_> {
    type Output = Packet;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _self = self.get_mut();
        let conn_manager = _self.conn_manager;

        // We found a buffered packet! Yay!
        if let Some(packet) = conn_manager.consume_conn_buf(_self.id) {
            return Poll::Ready(packet);
        }

        // We did not... let's peek into the socket and see if something is waiting for us
        match _self.reader.poll_read_packet(cx) {
            Poll::Ready(packet) => {
                let key_from_packet = format!(
                    "{}_{}",
                    packet.payload.header.source_connection_id,
                    packet.payload.header.destination_connection_id
                );

                // This is definitely not for me... we haven't even registered this connection yet.
                // Could be an new conenction request that we buffered in.
                if !conn_manager.conn_exists(&key_from_packet) {
                    // New connection requests should have the dst id to 0x0. Drop this packet.
                    if packet.payload.header.destination_connection_id != 0x0 {
                        conn_manager.add_waker_to_existing_conn(_self.id, cx.waker().clone());
                        return Poll::Pending;
                    }

                    // New connection requests should have type `UnencryptedHandshake`
                    if packet
                        .payload
                        .header
                        .type_and_type_specific_payload
                        .packet_type
                        != PacketType::UnencryptedHandshake
                    {
                        conn_manager.add_waker_to_existing_conn(_self.id, cx.waker().clone());
                        return Poll::Pending;
                    }

                    // This is looking like a new connection request! Good for them but not for me...
                    conn_manager.buffer_to_new_connection_request(packet);
                    conn_manager.add_waker_to_existing_conn(_self.id, cx.waker().clone());
                    return Poll::Pending;
                }

                // So this packet is indeed for an existing connection... just not this one. Buffer
                // it and wake.
                if key_from_packet != _self.id {
                    conn_manager.buffer_to_existing_connection(&key_from_packet, packet);
                    conn_manager.add_waker_to_existing_conn(_self.id, cx.waker().clone());
                    return Poll::Pending;
                }

                // So this connection has been registered AND it is indeed mine! Let's clean up and return.
                let _ = conn_manager.consume_conn_buf(key_from_packet);
                Poll::Ready(packet)
            }
            // Nothing yet... let's just register our waker and wait
            Poll::Pending => {
                conn_manager.add_waker_to_existing_conn(_self.id, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
