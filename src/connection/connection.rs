use std::{
    fs::File,
    io::{self, ErrorKind, Read, Write},
};

use etherparse::PacketBuilder;

use crate::core::context::{BluefinHost, Context, State};

/// A Bluefin `Connection`
pub struct Connection {
    pub id: String,
    need_ip_udp_headers: bool,
    pub connection_id: [u8; 4],
    raw_file: File,
    pub bytes_in: Option<Vec<u8>>,
    pub bytes_out: Option<Vec<u8>>,
    pub source_ip: Option<[u8; 4]>,
    pub destination_ip: Option<[u8; 4]>,
    pub source_port: Option<u16>,
    pub destination_port: Option<u16>,
    pub context: Context,
}

impl Connection {
    pub fn new(id: String, connection_id: [u8; 4], raw_file: File, host_type: BluefinHost) -> Self {
        Connection {
            id,
            connection_id,
            raw_file,
            need_ip_udp_headers: true,
            bytes_in: None,
            bytes_out: None,
            source_ip: None,
            source_port: None,
            destination_ip: None,
            destination_port: None,
            context: Context {
                host_type,
                state: State::Handshake,
            },
        }
    }

    pub fn set_bytes_in(&mut self, bytes_in: Vec<u8>) {
        self.bytes_in = Some(bytes_in);
    }

    pub fn set_bytes_out(&mut self, bytes_out: Vec<u8>) {
        self.bytes_out = Some(bytes_out);
    }

    pub fn need_ip_udp_headers(&mut self, need_ip_udp_headers: bool) {
        self.need_ip_udp_headers = need_ip_udp_headers;
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.raw_file.read(buf);

        self.set_bytes_in(buf.into());

        size
    }

    pub async fn write(&mut self) -> io::Result<usize> {
        if self.bytes_out.is_none() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Can't write zero bytes",
            ));
        }

        let bytes_out = self.bytes_out.as_ref().unwrap();

        // No need to pre-build; just flush buffer
        if !self.need_ip_udp_headers {
            return self.raw_file.write(bytes_out);
        }

        let packet_builder =
            PacketBuilder::ipv4(self.destination_ip.unwrap(), self.source_ip.unwrap(), 20)
                .udp(self.destination_port.unwrap(), self.source_port.unwrap());

        let mut tun_tap_bytes = vec![0, 0, 0, 2];
        let mut writer = Vec::<u8>::with_capacity(packet_builder.size(bytes_out.capacity()));
        let _ = packet_builder.write(&mut writer, &bytes_out);
        tun_tap_bytes.append(&mut writer);

        self.raw_file.write(&tun_tap_bytes)
    }

    pub async fn process(&mut self) {
        eprintln!(
            "Processing connection<{:?}> from {:?}:{}",
            self.id,
            self.source_ip.unwrap(),
            self.source_port.unwrap()
        );
        // Try writing something using raw socket fd
        let response = vec![1, 3, 1, 8, 2, 6];
        self.bytes_out = Some(response);

        self.write().await;
    }
}
