use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::{net::UdpSocket, sync::RwLock};

use super::{
    connection::{BluefinConnection, ConnectionBuffer, ConnectionManager},
    AckBuffer, ConnectionManagedBuffers,
};
use crate::utils::get_udp_socket;
use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, Serialisable},
    net::{
        build_and_start_tx, build_empty_encrypted_packet, connection::HandshakeConnectionBuffer,
    },
    utils::common::BluefinResult,
};

const NUM_TX_WORKERS_FOR_CLIENT_DEFAULT: u16 = 1;

pub struct BluefinClient {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    num_reader_workers: u16,
}

impl BluefinClient {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            dst_addr: None,
            conn_manager: Arc::new(RwLock::new(ConnectionManager::new())),
            src_addr,
            num_reader_workers: NUM_TX_WORKERS_FOR_CLIENT_DEFAULT,
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

    pub async fn connect(&mut self, dst_addr: SocketAddr) -> BluefinResult<BluefinConnection> {
        let socket = Arc::new(get_udp_socket(self.src_addr)?);
        self.socket = Some(Arc::clone(&socket));
        self.dst_addr = Some(dst_addr);

        build_and_start_tx(
            self.num_reader_workers,
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
            Arc::new(Mutex::new(Vec::new())),
            BluefinHost::Client,
        );

        let src_conn_id: u32 = rand::thread_rng().gen();
        let packet_number: u64 = rand::thread_rng().gen();
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::Client,
        )));
        let ack_buff = Arc::new(Mutex::new(AckBuffer::new(packet_number + 2)));
        let conn_mgrs_buffs = ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buff),
        };
        let handshake_buf = HandshakeConnectionBuffer::new(Arc::clone(&conn_buffer));

        // Register the connection
        let hello_key = format!("{}_0", src_conn_id);
        self.conn_manager
            .write()
            .await
            .insert(&hello_key, conn_mgrs_buffs.clone())?;

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
        let key = format!("{}_{}", src_conn_id, dst_conn_id);
        let server_packet_number = server_hello.header.packet_number;
        // Bluefin handshake asserts that the initial packet numbers cannot be zero
        if server_packet_number == 0x0 {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // delete the old hello entry and insert the new connection entry
        {
            let mut guard = self.conn_manager.write().await;
            let _ = guard.remove(&hello_key);
            let _ = guard.insert(&key, conn_mgrs_buffs);
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
            self.dst_addr.unwrap(),
            self.src_addr,
        ))
    }
}
