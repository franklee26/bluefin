use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::{net::UdpSocket, sync::RwLock};

use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, Serialisable},
    net::{
        build_and_start_tx, build_empty_encrypted_packet, connection::HandshakeConnectionBuffer,
    },
    utils::common::BluefinResult,
};

use super::connection::{BluefinConnection, ConnectionBuffer, ConnectionManager};

const NUM_TX_WORKERS_FOR_CLIENT: u8 = 5;

pub struct BluefinClient {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
}

impl BluefinClient {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            dst_addr: None,
            conn_manager: Arc::new(RwLock::new(ConnectionManager::new())),
            src_addr,
        }
    }

    pub async fn connect(&mut self, dst_addr: SocketAddr) -> BluefinResult<BluefinConnection> {
        let socket = Arc::new(UdpSocket::bind(self.src_addr).await?);
        socket.connect(dst_addr).await?;
        self.socket = Some(Arc::clone(&socket));
        self.dst_addr = Some(dst_addr);

        build_and_start_tx(
            NUM_TX_WORKERS_FOR_CLIENT,
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::new(Mutex::new(Vec::new())),
            BluefinHost::Client,
        );

        let src_conn_id: u32 = rand::thread_rng().gen();
        eprintln!("client src id: 0x{:x}", src_conn_id);
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::Client,
        )));
        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));

        // Register the connection
        let hello_key = format!("{}_0", src_conn_id);
        self.conn_manager
            .write()
            .await
            .insert(&hello_key, Arc::clone(&conn_buffer))?;

        // send the client hello
        let packet_number: u64 = rand::thread_rng().gen();
        let packet = build_empty_encrypted_packet(
            src_conn_id,
            0x0,
            packet_number,
            PacketType::UnencryptedClientHello,
        );
        self.socket
            .as_ref()
            .unwrap()
            .send(&packet.serialise())
            .await?;

        // Wait for server hello. This will timeout after 3s.
        let server_hello_timeout = Duration::from_secs(3);
        let (server_hello, _) = handshake_buf
            .read_with_timeout(server_hello_timeout)
            .await?;
        let dst_conn_id = server_hello.header.source_connection_id;
        let key = format!("{}_{}", src_conn_id, dst_conn_id);
        let server_packet_number = server_hello.header.packet_number;
        // Bluefin handshake asserts that the initial packet numbers cannot be zero
        if server_packet_number == 0x0 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // delete the old hello entry and insert the new connection entry
        let mut guard = self.conn_manager.write().await;
        let _ = guard.remove(&hello_key);
        let _ = guard.insert(&key, Arc::clone(&conn_buffer));
        drop(guard);

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
            .send(&packet.serialise())
            .await?;

        Ok(BluefinConnection::new(
            src_conn_id,
            dst_conn_id,
            packet_number + 2,
            Arc::clone(&conn_buffer),
            Arc::clone(self.socket.as_ref().unwrap()),
        ))
    }
}
