use std::{
    io::{self, ErrorKind},
    net::UdpSocket,
    os::fd::{FromRawFd, IntoRawFd},
};

use rand::distributions::{Alphanumeric, DistString};
use tokio::fs::File;

use crate::{
    connection::connection::Connection, core::context::BluefinHost,
    handshake::handshake::bluefin_handshake_handle,
};

pub struct BluefinClient {
    source_id: i32,
    name: String,
    raw_fd: Option<i32>,
}

pub struct BluefinClientBuilder {
    source_id: Option<i32>,
    name: Option<String>,
}

impl BluefinClient {
    pub fn builder() -> BluefinClientBuilder {
        BluefinClientBuilder {
            source_id: None,
            name: None,
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

        let mut conn = Connection::new(id, self.source_id, raw_file, BluefinHost::Client);
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

    pub fn build(&mut self) -> BluefinClient {
        BluefinClient {
            source_id: self.source_id.unwrap(),
            name: self.name.clone().unwrap(),
            raw_fd: None,
        }
    }
}
