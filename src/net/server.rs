use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use rand::Rng;
use tokio::{net::UdpSocket, spawn, sync::RwLock};

use crate::{
    core::{
        context::BluefinHost,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        Serialisable,
    },
    net::connection::HandshakeConnectionBuffer,
    utils::common::BluefinResult,
    worker::reader::TxChannel,
};

use super::connection::{BluefinConnection, ConnectionBuffer, ConnectionManager};

#[derive(Clone)]
pub struct BluefinServer {
    socket: Option<Arc<UdpSocket>>,
    bind_called: bool,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    client_hello_handled: bool,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
}

impl BluefinServer {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            dst_addr: None,
            bind_called: false,
            client_hello_handled: false,
            conn_manager: Arc::new(RwLock::new(ConnectionManager::new())),
            pending_accept_ids: Arc::new(Mutex::new(Vec::new())),
            src_addr,
        }
    }

    pub async fn bind(&mut self) -> BluefinResult<()> {
        let socket = UdpSocket::bind(self.src_addr).await?;
        self.socket = Some(Arc::new(socket));

        let tx = TxChannel::new(
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::clone(&self.pending_accept_ids),
            BluefinHost::PackLeader,
        );

        for i in 0..10 {
            let mut tx_clone = tx.clone();
            tx_clone.id = i;
            spawn(async move {
                let _ = tx_clone.run().await;
            });
        }

        self.bind_called = true;
        Ok(())
    }

    pub async fn accept(&mut self) -> BluefinResult<BluefinConnection> {
        // generate random conn id and insert buffer
        let src_conn_id: u32 = rand::thread_rng().gen();
        eprintln!("src_conn_id: {}", src_conn_id);
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new()));
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

        // delete the old hello entry and insert the new connection entry
        let mut guard = self.conn_manager.write().await;
        let _ = guard.remove(&hello_key);
        let _ = guard.insert(&key, Arc::clone(&conn_buffer));
        drop(guard);

        // send server hello
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::UnencryptedServerHello,
            0x0,
            security_fields,
        );
        let packet = BluefinPacket::builder().header(header).build();
        self.socket
            .as_ref()
            .unwrap()
            .send_to(&packet.serialise(), addr)
            .await?;

        // Wait for client ack
        let client_ack_packet = handshake_buf.read().await;

        Ok(BluefinConnection::new(
            src_conn_id,
            dst_conn_id,
            Arc::clone(&conn_buffer),
            Arc::clone(self.socket.as_ref().unwrap()),
        ))
    }
}
