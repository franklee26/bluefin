use crate::core::{context::BluefinHost, header::PacketType, packet::Packet};

pub mod client;
pub mod connection;
pub mod server;

/// Helper to determine whether a given `packet` is a valid hello packet eg. client-hello or pack-leader-hello
#[inline]
fn is_hello_packet(host_type: BluefinHost, packet: &Packet) -> bool {
    let header = packet.payload.header;
    let other_id = header.source_connection_id;
    let this_id = header.destination_connection_id;

    // if client-hello then, must have a non-zero source id
    if host_type == BluefinHost::PackLeader && other_id == 0x0 {
        return false;
    }

    // if client-hello then, the destination id must be 0x0
    if host_type == BluefinHost::PackLeader && this_id != 0x0 {
        return false;
    }

    // Client hellos' must have type UnencryptedHandshake
    if header.type_field != PacketType::UnencryptedHandshake {
        return false;
    }

    true
}
