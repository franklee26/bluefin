use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::{net::UdpSocket, sync::RwLock};

use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, Serialisable},
    net::{build_empty_encrypted_packet, connection::HandshakeConnectionBuffer},
    utils::common::BluefinResult,
};

use super::{
    build_and_start_tx,
    connection::{BluefinConnection, ConnectionBuffer, ConnectionManager},
};

const NUM_TX_WORKERS_FOR_SERVER: u8 = 10;

#[derive(Clone)]
pub struct BluefinServer {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
}

impl BluefinServer {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            conn_manager: Arc::new(RwLock::new(ConnectionManager::new())),
            pending_accept_ids: Arc::new(Mutex::new(Vec::new())),
            src_addr,
        }
    }

    pub async fn bind(&mut self) -> BluefinResult<()> {
        let socket = UdpSocket::bind(self.src_addr).await?;
        self.socket = Some(Arc::new(socket));

        build_and_start_tx(
            NUM_TX_WORKERS_FOR_SERVER,
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::clone(&self.pending_accept_ids),
            BluefinHost::PackLeader,
        );

        Ok(())
    }

    pub async fn accept(&mut self) -> BluefinResult<BluefinConnection> {
        // generate random conn id and insert buffer
        let src_conn_id: u32 = rand::thread_rng().gen();
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::PackLeader,
        )));
        let hello_key = format!("{}_0", src_conn_id);
        let _ = self
            .conn_manager
            .write()
            .await
            .insert(&hello_key, Arc::clone(&conn_buffer));
        self.pending_accept_ids.lock().unwrap().push(src_conn_id);

        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));
        let (packet, addr) = handshake_buf.read().await;
        let dst_conn_id = packet.header.source_connection_id;
        let key = format!("{}_{}", src_conn_id, dst_conn_id);
        let client_packet_num = packet.header.packet_number;

        // The packet number must be non-zero. Otherwise, we cannot accept the connection
        // and we finish with an error.
        if client_packet_num == 0 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // delete the old hello entry and insert the new connection entry
        let mut guard = self.conn_manager.write().await;
        let _ = guard.remove(&hello_key);
        let _ = guard.insert(&key, Arc::clone(&conn_buffer));
        drop(guard);

        // send server hello
        let packet_number: u64 = rand::thread_rng().gen();
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
            Arc::clone(self.socket.as_ref().unwrap()),
            addr,
        ))
    }
}
