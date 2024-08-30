use crate::core::{context::BluefinHost, header::PacketType, packet::BluefinPacket};

pub mod client;
pub mod connection;
pub mod server;

/// Helper to determine whether a given `packet` is a valid hello packet eg. client-hello or pack-leader-hello
#[inline]
pub(crate) fn is_hello_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    // For a server, the handshake must be initiated by an client hello
    if host_type == BluefinHost::PackLeader
        && packet.header.type_field != PacketType::UnencryptedClientHello
    {
        return false;
    }

    // For a client, the handshake must be followed up by an server hello
    if host_type == BluefinHost::Client
        && packet.header.type_field != PacketType::UnencryptedServerHello
    {
        return false;
    }

    // For a client receiving a server hello, both ids MUST be set
    if host_type == BluefinHost::Client && (other_id == 0x0 || this_id == 0x0) {
        return false;
    }

    // if handshake, must have a non-zero source id
    if host_type == BluefinHost::PackLeader && other_id == 0x0 {
        return false;
    }

    // if handshake, the destination id must be 0x0
    if host_type == BluefinHost::PackLeader && this_id != 0x0 {
        return false;
    }

    true
}

#[inline]
pub(crate) fn is_client_ack_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    if host_type == BluefinHost::PackLeader
        && packet.header.type_field == PacketType::Ack
        && other_id != 0x0
        && this_id != 0x0
    {
        return true;
    }
    false
}
