use std::sync::Arc;

use etherparse::PacketBuilder;
use tokio::fs::File;

use crate::{
    core::{context::BluefinHost, packet::Packet},
    io::{manager::ConnectionBuffer, stream_manager::ReadStreamManager},
    network::connection::Connection,
};

/// Synchronously locked - mutuably shared ConnectionBuffer reference
pub(crate) type SyncConnBufferRef = Arc<std::sync::Mutex<ConnectionBuffer>>;

/// Builds a connection from an client-hello packet. Note that this function does not actually validate
/// whether `packet` is in fact an valid client-hello. This is up to the invoker.
#[inline]
pub(crate) fn build_connection_from_packet(
    packet: &Packet,
    source_id: u32,
    host_type: BluefinHost,
    need_ip_and_udp_headers: bool,
    buffer: Arc<std::sync::Mutex<ConnectionBuffer>>,
    read_stream_manager: Arc<tokio::sync::Mutex<ReadStreamManager>>,
    file: File,
) -> Connection {
    let mut conn = Connection::new(
        // The destination id must be whatever the src is calling its src id
        packet.payload.header.source_connection_id as u32,
        packet.payload.header.packet_number,
        packet.dst_ip,
        packet.dst_port,
        packet.src_ip,
        packet.src_port,
        host_type,
        need_ip_and_udp_headers,
        file,
        buffer,
        read_stream_manager,
    );
    conn.source_id = source_id;

    conn
}

/// Converts a string representation of an ip address to a byte-vector representation
#[inline]
pub(crate) fn string_to_vec_ip(ip: &str) -> Vec<u8> {
    ip.split(".").map(|s| s.parse::<u8>().unwrap()).collect()
}

/// Metadata context required to build write-able packets
#[derive(Debug)]
pub(crate) struct WriteContext {
    pub(crate) need_ip_udp_headers: bool,
    pub(crate) src_ip: [u8; 4],
    pub(crate) dst_ip: [u8; 4],
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
}

/// Returns writeable bytes to the wire by preprending any tuntap + ip + udp headers (if required)
#[inline]
pub(crate) fn get_writeable_bytes(data: &[u8], context: WriteContext) -> Vec<u8> {
    // No need to pre-build; just flush buffer
    if !context.need_ip_udp_headers {
        return data.into();
    }

    let packet_builder = PacketBuilder::ipv4(context.src_ip, context.dst_ip, 20)
        .udp(context.src_port, context.dst_port);

    let mut tun_tap_bytes = vec![0, 0, 0, 2];
    let mut writer = Vec::<u8>::with_capacity(packet_builder.size(data.len()));
    let _ = packet_builder.write(&mut writer, data);
    tun_tap_bytes.append(&mut writer);

    tun_tap_bytes
}
