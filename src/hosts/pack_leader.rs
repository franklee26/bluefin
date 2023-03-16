use std::{
    fs::File,
    io::{self, ErrorKind, Read},
    os::fd::FromRawFd,
};

use crate::{
    connection::connection::Connection,
    core::context::{BluefinHost, State},
    handshake::handshake::bluefin_handshake_handle,
    tun::device::BluefinDevice,
};
use etherparse::{Ipv4Header, PacketHeaders};
use rand::distributions::{Alphanumeric, DistString};

pub struct BluefinPackLeader {
    source_id: [u8; 4],
    num_connections: usize,
    raw_file: File,
    name: String,
}

impl BluefinPackLeader {
    pub fn builder() -> BluefinPackLeaderBuilder {
        BluefinPackLeaderBuilder {
            source_id: None,
            name: None,
            bind_address: None,
            netmask: None,
        }
    }
}

pub struct BluefinPackLeaderBuilder {
    source_id: Option<[u8; 4]>,
    name: Option<String>,
    bind_address: Option<String>,
    netmask: Option<String>,
}

impl BluefinPackLeaderBuilder {
    pub fn source_id(mut self, source_id: [u8; 4]) -> Self {
        self.source_id = Some(source_id);
        self
    }

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

        let raw_fd = device.get_raw_fd();
        let raw_file = unsafe { File::from_raw_fd(raw_fd) };

        BluefinPackLeader {
            source_id: self.source_id.unwrap(),
            num_connections: 0,
            raw_file,
            name,
        }
    }
}

impl BluefinPackLeader {
    // Returns a Connection as a handle
    pub async fn accept(&mut self, buf: &mut [u8]) -> io::Result<Connection> {
        let num_bytes_read = self.raw_file.read(buf)?;
        eprintln!("BUF STATE: {:?}", &buf[..num_bytes_read]);

        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);

        // Build new connection handle
        let mut connection = Connection::new(
            id,
            self.source_id,
            self.raw_file.try_clone().unwrap(),
            BluefinHost::PackLeader,
        );
        connection.set_bytes_in(buf[..num_bytes_read].into());

        // Could not parse connection; fail here.
        self.parse_and_set_header_info(&mut connection)?;

        // Update connection count
        self.num_connections += 1;

        Ok(connection)
    }

    fn parse_and_set_header_info(&mut self, conn: &mut Connection) -> io::Result<()> {
        let bytes = conn.bytes_in.as_ref().unwrap();
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

                // Throw away the header
                conn.set_bytes_in(bytes[ip_header_len + 4 + udp_header_len..].into());

                // Now confirm that this is a bluefin packet
                if let Err(bluefin_err) = bluefin_handshake_handle(conn) {
                    conn.context.state = State::Error;
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!("Invalid packet. Aborting connection. {:?}", bluefin_err),
                    ));
                }

                Ok(())
            }
        }
    }
}
