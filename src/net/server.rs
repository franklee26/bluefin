use std::{
    mem,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use sysctl::Sysctl;
use tokio::{net::UdpSocket, sync::RwLock};

use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, Serialisable},
    net::{build_empty_encrypted_packet, connection::HandshakeConnectionBuffer},
    utils::common::BluefinResult,
};

use super::{
    build_and_start_tx,
    connection::{BluefinConnection, ConnectionBuffer, ConnectionManager},
    AckBuffer, ConnectionManagedBuffers,
};
use std::os::fd::AsRawFd;
const NUM_TX_WORKERS_FOR_SERVER_DEFAULT: u16 = 10;

#[derive(Clone)]
pub struct BluefinServer {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    num_reader_workers: u16,
}

impl BluefinServer {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            conn_manager: Arc::new(RwLock::new(ConnectionManager::new())),
            pending_accept_ids: Arc::new(Mutex::new(Vec::new())),
            src_addr,
            num_reader_workers: NUM_TX_WORKERS_FOR_SERVER_DEFAULT,
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

    pub async fn bind(&mut self) -> BluefinResult<()> {
        let socket = UdpSocket::bind(self.src_addr).await?;
        let socket_fd = socket.as_raw_fd();
        self.socket = Some(Arc::new(socket));

        #[cfg(target_os = "macos")]
        if let Ok(ctl) = sysctl::Ctl::new("net.inet.udp.maxdgram") {
            match ctl.set_value_string("16000") {
                Ok(s) => {
                    println!("Successfully set net.inet.udp.maxdgram to {}", s)
                }
                Err(e) => eprintln!("Failed to set net.inet.udp.maxdgram due to err: {:?}", e),
            }
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            let optval: libc::c_int = 1;
            let ret = libc::setsockopt(
                socket_fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                mem::size_of_val(&optval) as libc::socklen_t,
            );
            if ret != 0 {
                return Err(BluefinError::InvalidSocketError);
            }
        }

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
        let src_conn_id: u32 = rand::thread_rng().gen();
        // This is the packet number the server will begin using.
        let packet_number: u64 = rand::thread_rng().gen();
        let conn_buffer = Arc::new(Mutex::new(ConnectionBuffer::new(
            src_conn_id,
            BluefinHost::PackLeader,
        )));
        let ack_buffer = Arc::new(Mutex::new(AckBuffer::new(packet_number + 1)));
        let conn_mgr_buffers = ConnectionManagedBuffers {
            conn_buff: Arc::clone(&conn_buffer),
            ack_buff: Arc::clone(&ack_buffer),
        };

        let hello_key = format!("{}_0", src_conn_id);
        let _ = self
            .conn_manager
            .write()
            .await
            .insert(&hello_key, conn_mgr_buffers.clone());
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
        let _ = guard.insert(&key, conn_mgr_buffers);
        drop(guard);

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
            Arc::clone(self.socket.as_ref().unwrap()),
            addr,
        ))
    }
}
