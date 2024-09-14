use std::sync::{Arc, Mutex};

use connection::ConnectionManager;
use tokio::{net::UdpSocket, spawn, sync::RwLock};

use crate::{
    core::{
        context::BluefinHost,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
    },
    worker::reader::TxChannel,
};

pub mod client;
pub mod connection;
pub mod ordered_bytes;
pub mod server;

/// Helper to build `num_tx_workers` number of tx workers to run.
#[inline]
fn build_and_start_tx(
    num_tx_workers: u8,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    host_type: BluefinHost,
) {
    let tx = TxChannel::new(socket, conn_manager, pending_accept_ids, host_type);

    for id in 0..num_tx_workers {
        let mut tx_clone = tx.clone();
        tx_clone.id = id;
        spawn(async move {
            let _ = tx_clone.run().await;
        });
    }
}

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
        && packet.header.type_field == PacketType::ClientAck
        && other_id != 0x0
        && this_id != 0x0
    {
        return true;
    }
    false
}

#[inline]
pub(crate) fn build_empty_encrypted_packet(
    src_conn_id: u32,
    dst_conn_id: u32,
    packet_number: u64,
    packet_type: PacketType,
) -> BluefinPacket {
    let security_fields = BluefinSecurityFields::new(false, 0x0);
    let mut header =
        BluefinHeader::new(src_conn_id, dst_conn_id, packet_type, 0x0, security_fields);
    header.with_packet_number(packet_number);
    BluefinPacket::builder().header(header).build()
}

#[inline]
pub(crate) fn build_ack_packet(
    src_conn_id: u32,
    dst_conn_id: u32,
    base_packet_number_ack: u64,
    number_packets_to_ack: u16,
) -> BluefinPacket {
    let security_fields = BluefinSecurityFields::new(false, 0x0);
    let mut header = BluefinHeader::new(
        src_conn_id,
        dst_conn_id,
        PacketType::Ack,
        number_packets_to_ack,
        security_fields,
    );
    header.with_packet_number(base_packet_number_ack);
    BluefinPacket::builder().header(header).build()
}
