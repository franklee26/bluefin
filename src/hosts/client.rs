use std::{
    io::{self},
    net::UdpSocket,
    os::fd::{AsRawFd, FromRawFd},
    sync::Arc,
    time::Duration,
};

use rand::Rng;
use tokio::fs::File;

use crate::{
    core::{
        context::{BluefinHost, State},
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
        packet::BluefinPacket,
        serialisable::Serialisable,
    },
    handshake::handshake::HandshakeHandler,
    io::{
        manager::{ConnectionBuffer, ConnectionManager, Result},
        read::ReadWorker,
    },
    network::connection::Connection,
};

#[derive(Debug)]
pub struct BluefinClient {
    name: String,
    timeout: Duration,
    socket: Option<UdpSocket>,
    src_ip: Option<String>,
    src_port: Option<i32>,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    file: Option<File>,
    established_udp_connection: bool,
    spawned_read_threads: bool,
}

pub struct BluefinClientBuilder {
    name: Option<String>,
    timeout: Option<Duration>,
}

impl BluefinClient {
    pub fn builder() -> BluefinClientBuilder {
        BluefinClientBuilder {
            name: None,
            timeout: None,
        }
    }

    /// Creates and binds a UDP socket to `address:port`
    pub fn bind(&mut self, address: &str, port: i32) -> io::Result<()> {
        let socket = UdpSocket::bind(format!("{}:{}", address, port)).unwrap();
        self.socket = Some(socket);
        self.src_ip = Some(address.to_string());
        self.src_port = Some(port);
        Ok(())
    }

    fn get_client_hello_packet(&self, src_id: u32, packet_number: i64) -> BluefinPacket {
        let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
        let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

        // Temporarily set dest id to zero
        let mut header = BluefinHeader::new(src_id, 0x0, type_fields, security_fields);
        header.with_packet_number(packet_number);

        BluefinPacket::builder().header(header).build()
    }

    /// Request a bluefin connection to `address:port`.
    ///
    /// This function builds a Bluefin handshake packet and asynchronously begins
    /// the handshake with the pack-leader. If the handshake completes successfully
    /// then an `Connection` struct is returned.
    pub async fn connect(&mut self, address: &str, port: i32) -> Result<Connection> {
        if self.socket.is_none() {
            return Err(BluefinError::InvalidSocketError);
        }

        if !self.established_udp_connection {
            let socket = self.socket.as_ref().unwrap();
            let _ = socket.connect(format!("{}:{}", address, port));
            let fd = socket.as_raw_fd();
            let file = unsafe { File::from_raw_fd(fd) };
            self.file = Some(file);

            self.established_udp_connection = true;
        }

        if !self.spawned_read_threads {
            for _ in 0..2 {
                let mut worker = ReadWorker::new(
                    Arc::clone(&self.manager),
                    self.file.as_ref().unwrap().try_clone().await.unwrap(),
                    false,
                    BluefinHost::Client,
                );

                tokio::spawn(async move {
                    worker.run().await;
                });
            }

            self.spawned_read_threads = true;
        }

        let packet_number = rand::thread_rng().gen();
        let src_ip: Vec<u8> = self
            .src_ip
            .as_ref()
            .unwrap()
            .split(".")
            .map(|s| s.parse::<u8>().unwrap())
            .collect();

        let dst_ip: Vec<u8> = address
            .split(".")
            .map(|s| s.parse::<u8>().unwrap())
            .collect();

        let buffer = Arc::new(std::sync::Mutex::new(ConnectionBuffer::new()));
        let mut conn = Connection::new(
            0x0,
            packet_number,
            [src_ip[0], src_ip[1], src_ip[2], src_ip[3]],
            self.src_port.unwrap() as u16,
            [dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]],
            port as u16,
            BluefinHost::Client,
            false,
            self.file.as_ref().unwrap().try_clone().await.unwrap(),
            Arc::clone(&buffer),
        );
        conn.need_ip_udp_headers(false);

        let temp_key = format!("0_{}", conn.source_id);

        // Lock acquired
        {
            let mut manager = self.manager.lock().await;
            manager.register_new_connection(&temp_key, Arc::clone(&buffer));
        }
        // Lock released

        // Generate and send client-hello
        let client_hello = self.get_client_hello_packet(conn.source_id, packet_number);
        conn.set_bytes_out(client_hello.serialise());
        conn.write().await?;

        // Wait for pack-leader response. (This read will delete our first-read entry). Be generous with this read.
        let packet = conn
            .read_with_id_and_timeout(Duration::from_secs(10))
            .await
            .unwrap();
        let key = format!(
            "{}_{}",
            packet.payload.header.source_connection_id,
            packet.payload.header.destination_connection_id
        );
        conn.dest_id = packet.payload.header.source_connection_id;

        // register our 'real' connection
        // Lock acquired
        {
            let mut manager = self.manager.lock().await;
            manager.register_new_connection(&key, Arc::clone(&buffer));
        }
        // Lock released

        let mut handshake_handler = HandshakeHandler::new(&mut conn, packet, BluefinHost::Client);
        handshake_handler.handle().await?;

        conn.context.state = State::Ready;

        Ok(conn)
    }
}

impl BluefinClientBuilder {
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn build(&mut self) -> BluefinClient {
        let manager = ConnectionManager::new();
        BluefinClient {
            name: self.name.clone().unwrap(),
            timeout: self.timeout.unwrap_or(Duration::from_secs(10)),
            src_ip: None,
            src_port: None,
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            file: None,
            socket: None,
            established_udp_connection: false,
            spawned_read_threads: false,
        }
    }
}
