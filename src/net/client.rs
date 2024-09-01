use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::{net::UdpSocket, spawn, sync::RwLock, time::timeout};

use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, Serialisable},
    net::{build_empty_encrypted_packet, connection::HandshakeConnectionBuffer},
    utils::common::BluefinResult,
    worker::reader::TxChannel,
};

use super::connection::{BluefinConnection, ConnectionBuffer, ConnectionManager};

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

        let mut tx = TxChannel::new(
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::new(Mutex::new(Vec::new())),
            BluefinHost::Client,
        );

        spawn(async move {
            let _ = tx.run().await;
        });

        self.dst_addr = Some(dst_addr);

        let src_conn_id: u32 = rand::thread_rng().gen();
        eprintln!("client src id: {}", src_conn_id);
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new()));
        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));

        // Register the connection
        let hello_key = format!("{}_0", src_conn_id);
        self.conn_manager
            .write()
            .await
            .insert(&hello_key, Arc::clone(&conn_buffer))?;

        // send the client hello
        let packet =
            build_empty_encrypted_packet(src_conn_id, 0x0, PacketType::UnencryptedClientHello);
        self.socket
            .as_ref()
            .unwrap()
            .send(&packet.serialise())
            .await?;

        // Wait for server hello. This will timeout after 3s.
        let server_hello_timeout = Duration::from_secs(3);
        let res = timeout(server_hello_timeout, handshake_buf.read()).await;
        if let Err(_) = res {
            return Err(BluefinError::TimedOut(
                "Did not receive server hello in time".to_string(),
            ));
        }
        let (packet, _) = res.unwrap();
        let dst_conn_id = packet.header.source_connection_id;
        let key = format!("{}_{}", src_conn_id, dst_conn_id);

        // delete the old hello entry and insert the new connection entry
        let mut guard = self.conn_manager.write().await;
        let _ = guard.remove(&hello_key);
        let _ = guard.insert(&key, Arc::clone(&conn_buffer));
        drop(guard);

        // send the client ack
        let packet = build_empty_encrypted_packet(src_conn_id, dst_conn_id, PacketType::Ack);
        self.socket
            .as_ref()
            .unwrap()
            .send(&packet.serialise())
            .await?;

        Ok(BluefinConnection::new(
            src_conn_id,
            dst_conn_id,
            Arc::clone(&conn_buffer),
            Arc::clone(self.socket.as_ref().unwrap()),
        ))
    }
}
