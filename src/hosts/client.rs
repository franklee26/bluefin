use std::{
    io::{self, ErrorKind},
    net::UdpSocket,
    os::fd::{FromRawFd, IntoRawFd},
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::fs::File;

use crate::{
    core::context::{BluefinHost, State},
    handshake::handshake::bluefin_handshake_handle,
    io::manager::ConnectionManager,
    network::connection::Connection,
};

#[derive(Debug)]
pub struct BluefinClient {
    name: String,
    timeout: Duration,
    socket: Option<UdpSocket>,
    raw_fd: Option<i32>,
    src_ip: Option<String>,
    src_port: Option<i32>,
    conn_manager: Arc<Mutex<ConnectionManager>>,
    file: Option<Arc<Mutex<File>>>,
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
        let socket = UdpSocket::bind(format!("{}:{}", address, port))?;
        self.socket = Some(socket);
        self.src_ip = Some(address.to_string());
        self.src_port = Some(port);
        Ok(())
    }

    /// Request a bluefin connection to `address:port`.
    ///
    /// This function builds a Bluefin handshake packet and asynchronously begins
    /// the handshake with the pack-leader. If the handshake completes successfully
    /// then an `Connection` struct is returned.
    pub async fn connect(&mut self, address: &str, port: i32) -> io::Result<Connection> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                ErrorKind::Other,
                "No socket found. Ensure that client is binded to address.",
            ));
        }

        let socket = self.socket.as_ref().unwrap().try_clone().unwrap();
        socket
            .connect(format!("{}:{}", address, port))
            .expect("Could not connect to address/port");
        let fd = socket.into_raw_fd();
        self.raw_fd = Some(fd);

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

        let file = unsafe { File::from_raw_fd(fd) };
        self.file = Some(Arc::new(Mutex::new(file)));

        let mut conn = Connection::new(
            0x0,
            packet_number,
            [src_ip[0], src_ip[1], src_ip[2], src_ip[3]],
            self.src_port.unwrap() as u16,
            [dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]],
            port as u16,
            BluefinHost::Client,
            false,
            Arc::clone(self.file.as_ref().unwrap()),
            Arc::clone(&self.conn_manager),
        );
        conn.need_ip_udp_headers(false);
        // Generate source_id
        conn.source_id = rand::thread_rng().gen();
        eprintln!("Working on: {:#08x}", conn.source_id);

        // Finally, send the hello-client handshake
        if let Err(handshake_err) = bluefin_handshake_handle(&mut conn).await {
            return Err(io::Error::new(
                ErrorKind::NotConnected,
                format!(
                    "Failed to complete handshake with pack-leader: {}",
                    handshake_err
                ),
            ));
        }

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
            raw_fd: None,
            src_ip: None,
            src_port: None,
            conn_manager: Arc::new(Mutex::new(manager)),
            file: None,
            socket: None,
        }
    }
}
