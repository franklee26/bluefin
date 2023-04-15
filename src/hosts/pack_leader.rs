use std::{
    io::{self, ErrorKind},
    os::fd::FromRawFd,
    sync::Arc,
};

use crate::{
    core::{
        context::{BluefinHost, State},
        error::BluefinError,
        packet::BluefinPacket,
        serialisable::Serialisable,
    },
    handshake::handshake::bluefin_handshake_handle,
    io::{
        buffered_read::BufferedRead,
        manager::{ConnectionManager, Result},
        read::{AcceptWorker, ReadWorker},
    },
    network::connection::Connection,
    tun::device::BluefinDevice,
    utils::common::build_connection_from_packet,
};
use etherparse::{Ipv4Header, PacketHeaders};
use rand::Rng;
use tokio::fs::File;

const NUMBER_OF_WORKER_THREADS: usize = 2;

pub struct BluefinPackLeader {
    num_connections: usize,
    file: File,
    fd: i32,
    name: String,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    spawned_read_thread: bool,
}

impl BluefinPackLeader {
    pub fn builder() -> BluefinPackLeaderBuilder {
        BluefinPackLeaderBuilder {
            name: None,
            bind_address: None,
            netmask: None,
        }
    }
}

pub struct BluefinPackLeaderBuilder {
    name: Option<String>,
    bind_address: Option<String>,
    netmask: Option<String>,
}

impl BluefinPackLeaderBuilder {
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn bind_address(mut self, bind_address: String) -> Self {
        self.bind_address = Some(bind_address);
        self
    }

    pub fn netmask(mut self, netmask: String) -> Self {
        self.netmask = Some(netmask);
        self
    }

    pub fn build(&self) -> BluefinPackLeader {
        let name = self.name.clone().unwrap_or("utun3".to_string());
        let address = self.bind_address.clone().unwrap();
        let netmask = self.netmask.clone().unwrap();

        let device = BluefinDevice::builder()
            .name(name.to_string())
            .address(address.parse().unwrap())
            .netmask(netmask.parse().unwrap())
            .build();

        let fd = device.get_raw_fd();
        let file = unsafe { File::from_raw_fd(fd) };

        let manager = ConnectionManager::new();

        BluefinPackLeader {
            num_connections: 0,
            fd,
            file,
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            name,
            spawned_read_thread: false,
        }
    }
}

impl BluefinPackLeader {
    /// Trying to validate an incoming client-hello request. Try to parse and deserialise the request
    /// and validate its contents. If everything looks good then proceed with handshake, else return
    /// err.
    fn validate_client_request(&self, conn: &mut Connection, bytes: &[u8]) -> Result<()> {
        let packet = BluefinPacket::deserialise(bytes)?;
        // Just get the header; don't really care if there is a payload or not
        let header = packet.header;

        // The source (client) is our destination. Connection id's can never be zero.
        let dest_id = header.source_connection_id as u32;
        if dest_id == 0x0 {
            return Err(BluefinError::InvalidHeaderError(
                "Cannot have connection-id of zero".to_string(),
            ));
        }
        conn.dest_id = dest_id;

        // Set packet_number for context
        conn.context.packet_number = header.packet_number;
        Ok(())
    }

    /// Pack-leader accepts a bluefin connection request. This function return an `Accept` future
    /// which asynchronously reads incoming bytes. Upon a valid bluefin handshake request packet,
    /// and successful handshake completion, the future registers and creates a `Connection` instance.
    pub async fn accept(&mut self) -> Result<Connection> {
        // Receive initial connection status
        let source_id: u32 = rand::thread_rng().gen();

        // Spawn some read worker threads
        if !self.spawned_read_thread {
            for _ in 0..NUMBER_OF_WORKER_THREADS {
                let mut worker = ReadWorker::new(
                    Arc::clone(&self.manager),
                    self.file.try_clone().await.unwrap(),
                    true,
                    BluefinHost::PackLeader,
                );

                tokio::spawn(async move {
                    worker.run().await;
                });
            }

            self.spawned_read_thread = true;
        }

        // Spawn an Accept helper
        let accept_worker = AcceptWorker::new(Arc::clone(&self.manager));
        let accept_worker_join_handle = tokio::spawn(async move {
            accept_worker.run().await;
        });

        let key = format!("0_{}", source_id);

        // Lock acquired
        let buffer = {
            let mut manager = self.manager.lock().await;
            manager.register_new_accept_connection(&key)
        };
        // Lock released

        let read = BufferedRead::new(Arc::clone(&buffer));
        let packet = read.await;

        // Abort the accept worker thread, we already found a packet!
        accept_worker_join_handle.abort();
        let mut conn = build_connection_from_packet(
            &packet,
            source_id,
            BluefinHost::PackLeader,
            true,
            Arc::clone(&buffer),
            self.file.try_clone().await.unwrap(),
        );

        // Lock acquired
        {
            let mut manager = self.manager.lock().await;
            // generate new entry (no need to delete old entry, handled by worker job)
            manager.register_new_connection(&key, Arc::clone(&buffer));
        }
        // Lock released

        bluefin_handshake_handle(&mut conn).await?;

        Ok(conn)
    }

    async fn parse_and_set_header_info(
        &mut self,
        conn: &mut Connection,
        bytes: &[u8],
    ) -> io::Result<()> {
        match PacketHeaders::from_ip_slice(&bytes[4..]) {
            // TODO: Is this the right error?
            Err(_) => Err(io::Error::from(ErrorKind::Unsupported)),
            Ok(value) => {
                let ip_header_len = value.ip.unwrap().header_len();
                let (ip_header, _) = Ipv4Header::from_slice(&bytes[4..ip_header_len + 4]).unwrap();

                // Not udp. Pass.
                if ip_header.protocol != 0x11 {
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!(
                            "Cannot accept non-UDP IP packet (found protocol {:?})",
                            ip_header.protocol
                        ),
                    ));
                }

                let udp_header = value.transport.unwrap().udp().unwrap();
                let udp_header_len = udp_header.header_len();

                conn.source_ip = Some(ip_header.source);
                conn.destination_ip = Some(ip_header.destination);
                conn.source_port = Some(udp_header.source_port);
                conn.destination_port = Some(udp_header.destination_port);

                // Validate bluefin packet
                if let Err(validation_error) =
                    // Throw away the header leaving only a bluefin packet
                    self.validate_client_request(
                        conn,
                        &bytes[4 + ip_header_len + udp_header_len..],
                    )
                {
                    // TODO: Send error response.
                    conn.context.state = State::Error;
                    return Err(io::Error::new(ErrorKind::InvalidData, validation_error));
                }

                // Proceed with handshake
                if let Err(bluefin_err) = bluefin_handshake_handle(conn).await {
                    conn.write_error_message(bluefin_err.to_string().as_bytes())
                        .await?;
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!("Aborting connection. {:?}", bluefin_err),
                    ));
                }

                Ok(())
            }
        }
    }
}
