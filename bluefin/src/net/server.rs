use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{
    build_and_start_tx,
    connection::{BluefinConnection, ConnectionBuffer, ConnectionManager},
    AckBuffer, ConnectionManagedBuffers, DiagSender,
};
use crate::{
    core::{header::PacketType, Serialisable},
    net::{build_empty_encrypted_packet, connection::HandshakeConnectionBuffer},
    utils::get_udp_socket,
};
use bluefin_proto::context::BluefinHost;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use tokio::net::UdpSocket;
use rand::{rng, Rng};

const NUM_TX_WORKERS_FOR_SERVER_DEFAULT: u16 = 1;

pub struct BluefinServer {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    conn_manager: Arc<ConnectionManager>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    num_reader_workers: u16,
    diag_tx: Option<DiagSender>,
}

impl BluefinServer {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            conn_manager: Arc::new(dashmap::DashMap::new()),
            pending_accept_ids: Arc::new(Mutex::new(Vec::new())),
            src_addr,
            num_reader_workers: NUM_TX_WORKERS_FOR_SERVER_DEFAULT,
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

    /// Enable diagnostic event reporting. Call before `bind()`. The
    /// sender half is threaded into each accepted connection's internal
    /// workers; poll the corresponding receiver to see ACK events.
    #[inline]
    pub fn set_diagnostics(&mut self, tx: DiagSender) {
        self.diag_tx = Some(tx);
    }

    pub async fn bind(&mut self) -> BluefinResult<()> {
        let socket = get_udp_socket(self.src_addr)?;
        self.socket = Some(Arc::new(socket));

        build_and_start_tx(
            self.num_reader_workers,
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::clone(&self.pending_accept_ids),
            BluefinHost::PackLeader,
        );

        Ok(())
    }

    pub async fn accept(&mut self) -> BluefinResult<BluefinConnection> {
        // generate random conn id and insert buffer
        let src_conn_id: u32 = rng().random();
        // This is the packet number the server will begin using.
        let packet_number: u64 = rng().random();
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::PackLeader,
        )));
        let ack_buffer = Arc::new(Mutex::new(AckBuffer::new(packet_number + 1)));
        let conn_mgr_buffers = ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buffer),
            diag_tx: self.diag_tx.clone(),
        };

        let hello_key = (src_conn_id, 0);
        if self.conn_manager.insert(hello_key, conn_mgr_buffers.clone()).is_some() {
            return Err(BluefinError::ConnectionAlreadyExists);
        }
        self.pending_accept_ids.lock().unwrap().push(src_conn_id);

        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));
        let (packet, addr) = handshake_buf.read().await;
        let dst_conn_id = packet.header.source_connection_id;
        let key = (src_conn_id, dst_conn_id);
        let client_packet_num = packet.header.packet_number;

        // The packet number must be non-zero. Otherwise, we cannot accept the connection
        // and we finish with an error.
        if client_packet_num == 0 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // delete the old hello entry and insert the new connection entry
        if self.conn_manager.remove(&hello_key).is_none() {
            return Err(BluefinError::NoSuchConnectionError);
        }
        if self.conn_manager.insert(key, conn_mgr_buffers).is_some() {
            return Err(BluefinError::ConnectionAlreadyExists);
        }

        // send server hello
        let packet = build_empty_encrypted_packet(
            src_conn_id,
            dst_conn_id,
            packet_number,
            PacketType::UnencryptedServerHello,
        );
        self.socket
            .as_ref()
            .unwrap()
            .send_to(&packet.serialise(), addr)
            .await?;

        // Wait for client ack. This will timeout after 3s
        let client_ack_timeout = Duration::from_secs(3);
        let (client_ack, _) = handshake_buf.read_with_timeout(client_ack_timeout).await?;
        // Expect the client ack correctly returns the packet number
        if client_ack.header.packet_number != client_packet_num + 1 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        Ok(BluefinConnection::new(
            src_conn_id,
            dst_conn_id,
            packet_number + 1,
            Arc::clone(&conn_buffer),
            Arc::clone(&ack_buffer),
            addr,
            self.src_addr,
            self.diag_tx.clone(),
        ))
    }
}
