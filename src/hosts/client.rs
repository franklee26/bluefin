use std::{
    io::{self, ErrorKind},
    net::UdpSocket,
    os::fd::{FromRawFd, IntoRawFd},
    time::{self, Duration},
};

use rand::distributions::{Alphanumeric, DistString};
use tokio::fs::File;

use crate::{
    core::context::{BluefinHost, State},
    handshake::handshake::bluefin_handshake_handle,
    network::connection::Connection,
};

pub struct BluefinClient {
    source_id: i32,
    name: String,
    timeout: Duration,
    raw_fd: Option<i32>,
}

pub struct BluefinClientBuilder {
    source_id: Option<i32>,
    name: Option<String>,
    timeout: Option<Duration>,
}

impl BluefinClient {
    pub fn builder() -> BluefinClientBuilder {
        BluefinClientBuilder {
            source_id: None,
            name: None,
            timeout: None,
        }
    }

    /// Creates and binds a UDP socket to `address:port`
    pub fn bind(&mut self, address: &str, port: i32) -> io::Result<()> {
        let socket = UdpSocket::bind(format!("{}:{}", address, port))?;
        self.raw_fd = Some(socket.into_raw_fd());
        Ok(())
    }

    /// Request a bluefin connection to `address:port`.
    ///
    /// This function builds a Bluefin handshake packet and asynchronously begins
    /// the handshake with the pack-leader. If the handshake completes successfully
    /// then an `Connection` struct is returned.
    pub async fn connect(&mut self, address: &str, port: i32) -> io::Result<Connection> {
        if self.raw_fd.is_none() {
            return Err(io::Error::new(
                ErrorKind::Other,
                "No socket found. Ensure that client is binded to address.",
            ));
        }

        let socket = unsafe { UdpSocket::from_raw_fd(self.raw_fd.unwrap()) };
        socket
            .connect(format!("{}:{}", address, port))
            .expect("Could not connect to address/port");
        self.raw_fd = Some(socket.into_raw_fd());
        let raw_file = unsafe { File::from_raw_fd(self.raw_fd.unwrap()) };

        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);

        let mut conn = Connection::new(
            id,
            self.source_id,
            raw_file,
            BluefinHost::Client,
            self.timeout,
        );
        conn.need_ip_udp_headers(false);

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
    pub fn source_id(mut self, source_id: i32) -> Self {
        self.source_id = Some(source_id);
        self
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn build(&mut self) -> BluefinClient {
        BluefinClient {
            source_id: self.source_id.unwrap(),
            name: self.name.clone().unwrap(),
            timeout: self.timeout.unwrap_or(Duration::from_secs(10)),
            raw_fd: None,
        }
    }
}
