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
        context::BluefinHost,
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        serialisable::Serialisable,
    },
    handshake::handshake::HandshakeHandler,
    io::{
        manager::{ConnectionBuffer, ConnectionManager},
        stream_manager::StreamManager,
        worker::ReadWorker,
        Result,
    },
    network::connection::Connection,
    utils::common::string_to_vec_ip,
};

use super::NUMBER_OF_WORKER_THREADS;

pub struct BluefinClient {
    name: String,
    socket: Option<UdpSocket>,
    src_ip: Option<String>,
    src_port: Option<u16>,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    stream_manager: Arc<tokio::sync::Mutex<StreamManager>>,
    file: Option<File>,
    established_udp_connection: bool,
    spawned_read_threads: bool,
}

pub struct BluefinClientBuilder {
    name: Option<String>,
}

impl BluefinClient {
    pub fn builder() -> BluefinClientBuilder {
        BluefinClientBuilder { name: None }
    }

    /// Creates and binds a UDP socket to `address:port`
    #[inline]
    pub fn bind(&mut self, address: &str, port: u16) -> io::Result<()> {
        let socket = UdpSocket::bind(format!("{}:{}", address, port)).unwrap();
        self.socket = Some(socket);
        self.src_ip = Some(address.to_string());
        self.src_port = Some(port);
        Ok(())
    }

    #[inline]
    fn get_client_hello_packet(&self, src_id: u32, packet_number: u64) -> BluefinPacket {
        let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

        // Temporarily set dest id to zero
        let mut header = BluefinHeader::new(
            src_id,
            0x0,
            PacketType::UnencryptedHandshake,
            0x0,
            security_fields,
        );
        header.with_packet_number(packet_number);

        BluefinPacket::builder().header(header).build()
    }

    /// Spawns some reader-worker threads. These threads will only get spawned at first
    /// invocation of `accept` and will run forever until the main thread has completed.
    #[inline]
    async fn spawn_read_threads(&mut self) {
        if self.spawned_read_threads {
            return;
        }

        for _ in 0..NUMBER_OF_WORKER_THREADS {
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

    #[inline]
    fn establish_udp_conn(&mut self, address: &str, port: u16) -> Result<()> {
        if self.socket.is_none() {
            return Err(BluefinError::InvalidSocketError);
        }

        if self.established_udp_connection {
            return Ok(());
        }

        let socket = self.socket.as_ref().unwrap();
        let _ = socket.connect(format!("{}:{}", address, port));
        let fd = socket.as_raw_fd();
        let file = unsafe { File::from_raw_fd(fd) };
        self.file = Some(file);

        self.established_udp_connection = true;
        Ok(())
    }

    /// Request a bluefin connection to `address:port`.
    ///
    /// This function builds a Bluefin handshake packet and asynchronously begins
    /// the handshake with the pack-leader. If the handshake completes successfully
    /// then an `Connection` struct is returned.
    pub async fn connect(&mut self, address: &str, port: u16) -> Result<Connection> {
        self.establish_udp_conn(address, port)?;
        self.spawn_read_threads().await;

        let packet_number: u64 = rand::thread_rng().gen();
        let src_ip = string_to_vec_ip(self.src_ip.as_ref().unwrap());
        let dst_ip = string_to_vec_ip(address);

        let buffer = Arc::new(std::sync::Mutex::new(ConnectionBuffer::new()));
        let mut conn = Connection::new(
            0x0,
            packet_number,
            src_ip.try_into().unwrap(),
            self.src_port.unwrap(),
            dst_ip.try_into().unwrap(),
            port,
            BluefinHost::Client,
            false,
            self.file.as_ref().unwrap().try_clone().await.unwrap(),
            Arc::clone(&buffer),
            Arc::clone(&self.stream_manager),
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
        conn.write(&client_hello.serialise()).await?;

        // Wait for pack-leader response. (This read will delete our first-read entry). Be generous with this read.
        let packet = conn
            .read_with_timeout_and_retries(Duration::from_secs(5), 3)
            .await?;
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

        Ok(conn)
    }
}

impl BluefinClientBuilder {
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn build(&mut self) -> BluefinClient {
        let manager = ConnectionManager::new();
        let stream_manager = StreamManager::new();

        BluefinClient {
            name: self.name.clone().unwrap(),
            src_ip: None,
            src_port: None,
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            stream_manager: Arc::new(tokio::sync::Mutex::new(stream_manager)),
            file: None,
            socket: None,
            established_udp_connection: false,
            spawned_read_threads: false,
        }
    }
}
