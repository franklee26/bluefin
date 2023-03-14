use std::{
    fs::File,
    io::{self, ErrorKind, Read},
    os::fd::FromRawFd,
};

use crate::{core::connection::Connection, tun::device::BluefinDevice};
use etherparse::{Ipv4Header, PacketHeaders};
use rand::distributions::{Alphanumeric, DistString};

pub struct BluefinPackLeader {
    source_id: [u8; 4],
    num_connections: usize,
    raw_file: File,
}

pub struct BluefinPackLeaderBuilder {
    source_id: Option<[u8; 4]>,
}

impl BluefinPackLeaderBuilder {
    pub fn builder() -> Self {
        Self { source_id: None }
    }

    pub fn source_id(mut self, source_id: [u8; 4]) -> Self {
        self.source_id = Some(source_id);
        self
    }

    pub fn build(&self) -> BluefinPackLeader {
        // TODO: change hardcode
        let name = "utun3";
        let address = "192.168.55.2";
        let netmask = "255.255.255.0";

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
        }
    }
}

impl BluefinPackLeader {
    // Returns a Connection as a handle
    pub async fn accept(&mut self, buf: &mut [u8]) -> Result<Connection, ()> {
        // let _ = self.device.read(buf).unwrap();

        let _ = self.raw_file.read(buf);
        eprintln!("BUF STATE: {:?}", buf);

        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);

        // Build new connection handle
        let mut connection =
            Connection::new(id, self.source_id, self.raw_file.try_clone().unwrap());
        connection.set_bytes_in(buf.into());

        // Could not parse connection; fail here.
        if let Err(_) = self.parse_and_set_header_info(buf, &mut connection) {
            return Err(());
        }

        // Update connection count
        self.num_connections += 1;

        Ok(connection)
    }

    fn parse_and_set_header_info(
        &mut self,
        buf: &mut [u8],
        conn: &mut Connection,
    ) -> io::Result<()> {
        match PacketHeaders::from_ip_slice(&buf[4..]) {
            // TODO: Is this the right error?
            Err(_) => Err(io::Error::from(ErrorKind::Unsupported)),
            Ok(value) => {
                let ip_header_len = value.ip.unwrap().header_len();
                let (ip_header, _) = Ipv4Header::from_slice(&buf[4..ip_header_len + 4]).unwrap();

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

                conn.source_ip = Some(ip_header.source);
                conn.destination_ip = Some(ip_header.destination);
                conn.source_port = Some(udp_header.source_port);
                conn.destination_port = Some(udp_header.destination_port);

                Ok(())
            }
        }
    }
}
