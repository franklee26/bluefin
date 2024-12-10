use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use ack_handler::{AckBuffer, AckConsumer};
use connection::{ConnectionBuffer, ConnectionManager};
use tokio::{net::UdpSocket, spawn, sync::RwLock};

use crate::{
    core::{
        context::BluefinHost,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
    },
    utils::{common::BluefinResult, get_connected_udp_socket},
    worker::{
        conn_reader::ConnReaderHandler,
        reader::ReaderTxChannel,
        writer::{WriterQueue, WriterRxChannel},
    },
};

pub mod ack_handler;
pub mod client;
pub mod connection;
pub mod ordered_bytes;
pub mod server;

pub(crate) const BLUEFIN_HEADER_SIZE_BYTES: usize = 20;
pub(crate) const MAX_BLUEFIN_PAYLOAD_SIZE_BYTES: usize = 1500;
pub(crate) const MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM: usize = 10;
pub(crate) const MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM: usize = MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM
    * (BLUEFIN_HEADER_SIZE_BYTES + MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);

#[derive(Clone)]
pub(crate) struct ConnectionManagedBuffers {
    pub(crate) conn_buff: Arc<Mutex<ConnectionBuffer>>,
    pub(crate) ack_buff: Arc<Mutex<AckBuffer>>,
}

/// Helper to build `num_tx_workers` number of tx workers to run.
#[inline]
fn build_and_start_tx(
    num_tx_workers: u16,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    host_type: BluefinHost,
) {
    let tx = ReaderTxChannel::new(socket, conn_manager, pending_accept_ids, host_type);

    for id in 0..num_tx_workers {
        let mut tx_clone = tx.clone();
        tx_clone.id = id;
        spawn(async move {
            let _ = tx_clone.run().await;
        });
    }
}

#[inline]
fn build_and_start_conn_reader_tx_channels(
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
    conn_bufs: Arc<ConnectionManagedBuffers>,
) -> BluefinResult<()> {
    let conn_socket = Arc::new(get_connected_udp_socket(src_addr, dst_addr)?);
    let handler = ConnReaderHandler::new(conn_socket, conn_bufs);
    handler.start()
}

#[inline]
fn build_and_start_writer_rx_channel(
    data_queue: Arc<Mutex<WriterQueue>>,
    ack_queue: Arc<Mutex<WriterQueue>>,
    socket: Arc<UdpSocket>,
    num_ack_consumer_workers: u8,
    dst_addr: SocketAddr,
    ack_buffer: Arc<Mutex<AckBuffer>>,
    next_packet_num: u64,
    src_conn_id: u32,
    dst_conn_id: u32,
) {
    let largest_recv_acked_packet_num = Arc::new(RwLock::new(0));
    let mut rx = WriterRxChannel::new(
        data_queue,
        ack_queue,
        socket,
        dst_addr,
        next_packet_num,
        src_conn_id,
        dst_conn_id,
    );
    let mut cloned = rx.clone();
    spawn(async move {
        cloned.run_data().await;
    });
    spawn(async move {
        rx.run_ack().await;
    });
    let ack_consumer = AckConsumer::new(Arc::clone(&ack_buffer), largest_recv_acked_packet_num);

    for _ in 0..num_ack_consumer_workers {
        let ack_consumer_clone = ack_consumer.clone();
        spawn(async move {
            ack_consumer_clone.run().await;
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
