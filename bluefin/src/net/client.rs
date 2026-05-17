use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{
    connection::{BluefinConnection, ConnectionBuffer, ConnectionManager},
    AckBuffer, ConnectionManagedBuffers, DiagSender,
};
use crate::utils::get_udp_socket;
use crate::{
    core::{header::PacketType, Serialisable},
    net::{
        build_and_start_tx, build_empty_encrypted_packet, connection::HandshakeConnectionBuffer,
    },
};
use bluefin_proto::context::BluefinHost;
use bluefin_proto::endpoint::Endpoint;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use rand::Rng;
use tokio::net::UdpSocket;

const NUM_TX_WORKERS_FOR_CLIENT_DEFAULT: u16 = 1;

pub struct BluefinClient {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    conn_manager: Arc<ConnectionManager>,
    num_reader_workers: u16,
    diag_tx: Option<DiagSender>,
}

impl BluefinClient {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            dst_addr: None,
            conn_manager: Arc::new(dashmap::DashMap::new()),
            src_addr,
            num_reader_workers: NUM_TX_WORKERS_FOR_CLIENT_DEFAULT,
            diag_tx: None,
        }
    }

    #[inline]
    pub fn set_num_reader_workers(&mut self, num_reader_workers: u16) -> BluefinResult<()> {
        if num_reader_workers == 0 {
            return Err(BluefinError::Unexpected(
                "Cannot have zero reader values".to_string(),
            ));
        }
        self.num_reader_workers = num_reader_workers;
        Ok(())
    }

    /// Enable diagnostic event reporting. Call before `connect()`. The
    /// sender half is threaded into the connection's internal workers;
    /// poll the corresponding receiver to see ACK events.
    #[inline]
    pub fn set_diagnostics(&mut self, tx: DiagSender) {
        self.diag_tx = Some(tx);
    }

    pub async fn connect(&mut self, dst_addr: SocketAddr) -> BluefinResult<BluefinConnection> {
        let socket = Arc::new(get_udp_socket(self.src_addr)?);
        self.socket = Some(Arc::clone(&socket));
        self.dst_addr = Some(dst_addr);

        build_and_start_tx(
            self.num_reader_workers,
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::new(Mutex::new(Endpoint::new(BluefinHost::Client))),
            BluefinHost::Client,
        );

        let src_conn_id: u32 = rand::rng().random();
        let packet_number: u64 = rand::rng().random();
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::Client,
        )));
        let ack_buff = Arc::new(Mutex::new(AckBuffer::new(packet_number + 2)));
        let peer_fin_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let close_buff = Arc::new(Mutex::new(
            crate::net::close_handler::CloseBuffer::new(Arc::clone(&peer_fin_observed)),
        ));
        let conn_mgrs_buffs = ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buff),
            close_buff: Arc::clone(&close_buff),
            peer_fin_observed: Arc::clone(&peer_fin_observed),
            fin_ack_tx: None,
            diag_tx: self.diag_tx.clone(),
        };
        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));

        // Register the connection
        let hello_key = (src_conn_id, 0);
        if self
            .conn_manager
            .insert(hello_key, conn_mgrs_buffs.clone())
            .is_some()
        {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        // send the client hello
        let packet = build_empty_encrypted_packet(
            src_conn_id,
            0x0,
            packet_number,
            PacketType::UnencryptedClientHello,
        );
        self.socket
            .as_ref()
            .unwrap()
            .send_to(&packet.serialise(), dst_addr)
            .await?;

        // Wait for server hello. This will timeout after 3s.
        let server_hello_timeout = Duration::from_secs(3);
        let (server_hello, _) = handshake_buf
            .read_with_timeout(server_hello_timeout)
            .await?;
        let dst_conn_id = server_hello.header.source_connection_id;
        let key = (src_conn_id, dst_conn_id);
        let server_packet_number = server_hello.header.packet_number;
        // Bluefin handshake asserts that the initial packet numbers cannot be zero
        if server_packet_number == 0x0 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // delete the old hello entry and insert the new connection entry
        if self.conn_manager.remove(&hello_key).is_none() {
            return Err(BluefinError::NoSuchConnectionError);
        }
        if self.conn_manager.insert(key, conn_mgrs_buffs).is_some() {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        // send the client ack
        let packet = build_empty_encrypted_packet(
            src_conn_id,
            dst_conn_id,
            packet_number + 1,
            PacketType::ClientAck,
        );
        self.socket
            .as_ref()
            .unwrap()
            .send_to(&packet.serialise(), dst_addr)
            .await?;

        Ok(BluefinConnection::new(
            src_conn_id,
            dst_conn_id,
            packet_number + 2,
            Arc::clone(&conn_buffer),
            Arc::clone(&ack_buff),
            Arc::clone(&close_buff),
            Arc::clone(&peer_fin_observed),
            Arc::clone(&self.conn_manager),
            self.dst_addr.unwrap(),
            self.src_addr,
            self.diag_tx.clone(),
        ))
    }
}
