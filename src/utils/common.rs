use std::sync::Arc;

use tokio::fs::File;

use crate::{
    core::{context::BluefinHost, packet::Packet},
    io::manager::ConnectionBuffer,
    network::connection::Connection,
};

/// Synchronously locked - mutuably shared ConnectionBuffer reference
pub(crate) type SyncConnBufferRef = Arc<std::sync::Mutex<ConnectionBuffer>>;

/// Builds a connection from an client-hello packet. Note that this function does not actually validate
/// whether `packet` is in fact an valid client-hello. This is up to the invoker.
pub(crate) fn build_connection_from_packet(
    packet: &Packet,
    source_id: u32,
    host_type: BluefinHost,
    need_ip_and_udp_headers: bool,
    buffer: Arc<std::sync::Mutex<ConnectionBuffer>>,
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
    );
    conn.source_id = source_id;

    conn
}
