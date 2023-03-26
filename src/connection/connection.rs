use std::{
    fmt,
    io::{self, ErrorKind},
    time::Duration,
};

use etherparse::{PacketBuilder, PacketHeaders};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

use crate::core::{
    context::{BluefinHost, Context, State},
    error::BluefinError,
    header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
    packet::BluefinPacket,
    serialisable::Serialisable,
};

/// A Bluefin `Connection`. This struct represents an established Bluefin connection
/// and keeps track of the state of the connection, input/output buffers and other
/// misc. Bluefin connection contexts.
///
/// Each `Connection` is unique and is identified by the `id` field.
#[derive(Debug)]
pub struct Connection {
    pub id: String,
    need_ip_udp_headers: bool,
    pub source_id: i32,
    pub dest_id: i32,
    raw_file: File,
    pub bytes_out: Option<Vec<u8>>,
    pub source_ip: Option<[u8; 4]>,
    pub destination_ip: Option<[u8; 4]>,
    pub source_port: Option<u16>,
    pub destination_port: Option<u16>,
    pub context: Context,
}

/// Read result contains the read bluefin packet along with additional metadata
/// such as the size of the read bytestream and any offset.
pub struct BluefinReadResult {
    pub size: usize,
    pub offset: usize,
    pub packet: BluefinPacket,
}

impl fmt::Display for Connection {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "BluefinConnection<{}> ({:?})\nsrc_id: {:#08x}\ndst_id: {:#08x}",
            self.id, self.context.state, self.source_id, self.dest_id
        )
    }
}

impl Connection {
    pub fn new(id: String, source_id: i32, raw_file: File, host_type: BluefinHost) -> Self {
        Connection {
            id,
            source_id,
            dest_id: 0x0,
            raw_file,
            need_ip_udp_headers: true,
            bytes_out: None,
            source_ip: None,
            source_port: None,
            destination_ip: None,
            destination_port: None,
            context: Context {
                host_type,
                state: State::Handshake,
                packet_number: 0x0,
            },
        }
    }

    pub fn set_bytes_out(&mut self, bytes_out: Vec<u8>) {
        self.bytes_out = Some(bytes_out);
    }

    pub fn need_ip_udp_headers(&mut self, need_ip_udp_headers: bool) {
        self.need_ip_udp_headers = need_ip_udp_headers;
    }

    /// This function is similar to `read` except:
    ///     1. Return value is wrapped with a `Result<BluefinReadResult, BluefinError>` instead of
    ///        io::Result<>. This makes it easier to deal with bluefin-specific errors.
    ///     2. This function is expecting the input to be a bluefin packet and not arbitrary bytes. If
    ///        the incoming bytes can not be deserialised into a bluefin packet then this errors.
    ///     3. No need to provide a buffer; we internally malloc 1504 byte buffer. This means the invoker
    ///        does not necessarily need to work with buffer ptr arithmetic.
    pub async fn bluefin_read_packet(&mut self) -> Result<BluefinReadResult, BluefinError> {
        let mut buf = vec![0; 1504];

        match self.read(&mut buf).await {
            Ok((offset, size)) => {
                let packet = BluefinPacket::deserialise(&buf[offset..offset + size])?;
                Ok(BluefinReadResult {
                    size,
                    offset,
                    packet,
                })
            }
            Err(err) => Err(BluefinError::ReadError(err.to_string())),
        }
    }

    /// read has a built in timeout of 10 sec. If the future does not yield a
    /// result in thie timeout period, then this function returns an error.
    /// Otherwise, the exact read contents are stored in the conn.bytes_in.
    ///
    /// Returns an offset and size pair
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<(usize, usize)> {
        let mut size = timeout(Duration::from_secs(10), self.raw_file.read(buf)).await??;

        // If I don't need to worry about ip/udp headers then I'm using a UDP socket. This means
        // that I'm just getting the Bluefin packet. Easy.
        if !self.need_ip_udp_headers {
            return Ok((0, size));
        }

        // Else, I need to worry about the additional ip + udp headers. Let's strip them and
        // return.
        let ip_packet = PacketHeaders::from_ip_slice(&buf[4..]).unwrap();
        let ip_header_len = ip_packet.ip.unwrap().header_len();
        let udp_header_len = ip_packet.transport.unwrap().udp().unwrap().header_len();

        let offset = 4 + ip_header_len + udp_header_len;
        size -= offset;

        Ok((offset, size))
    }

    /// Asynchronously reads in bytes from connection into buf. This func is the same
    /// as `read` but does not have a timeout.
    pub async fn read_without_timeout(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.raw_file.read(buf).await?;

        Ok(size)
    }

    pub async fn write_error_message(&mut self, err_msg: &[u8]) -> io::Result<()> {
        let mut packet = self.get_packet(Some(err_msg.into()));
        packet.header.type_and_type_specific_payload.packet_type = PacketType::Error;
        self.context.state = State::Error;

        self.raw_file.write_all(&packet.serialise()).await?;
        Ok(())
    }

    pub async fn write(&mut self) -> io::Result<()> {
        if self.bytes_out.is_none() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Can't write zero bytes",
            ));
        }

        let bytes_out = self.bytes_out.as_ref().unwrap();

        // No need to pre-build; just flush buffer
        if !self.need_ip_udp_headers {
            return self.raw_file.write_all(bytes_out).await;
        }

        let packet_builder =
            PacketBuilder::ipv4(self.destination_ip.unwrap(), self.source_ip.unwrap(), 20)
                .udp(self.destination_port.unwrap(), self.source_port.unwrap());

        let mut tun_tap_bytes = vec![0, 0, 0, 2];
        let mut writer = Vec::<u8>::with_capacity(packet_builder.size(bytes_out.capacity()));
        let _ = packet_builder.write(&mut writer, &bytes_out);
        tun_tap_bytes.append(&mut writer);

        self.bytes_out = None;
        self.raw_file.write_all(&tun_tap_bytes).await
    }

    pub fn get_packet(&self, payload: Option<Vec<u8>>) -> BluefinPacket {
        let packet_type = match self.context.state {
            State::Handshake => PacketType::UnencryptedHandshake,
            State::Error => PacketType::Error,
            State::DataStream => PacketType::Data,
            // We should never try to build an empty packet if the connection is closed
            State::Closed => unreachable!(),
        };
        // TODO: additional type specific fields?
        let type_fields = BluefinTypeFields::new(packet_type, 0x0);
        // TODO: tls encryption + mask?
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header =
            BluefinHeader::new(self.source_id, self.dest_id, type_fields, security_fields);
        header.with_packet_number(self.context.packet_number);
        if payload.is_some() {
            let packet = BluefinPacket::builder()
                .header(header)
                .payload(payload.unwrap())
                .build();
            return packet;
        }
        let packet = BluefinPacket::builder().header(header).build();
        packet
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
        let packet = self.get_packet(Some(response));
        self.bytes_out = Some(packet.serialise());

        self.write().await;
    }
}
