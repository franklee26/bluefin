use std::{
    fmt,
    io::{self, ErrorKind},
    sync::{Arc, Mutex},
    time::Duration,
};

use etherparse::{PacketBuilder, PacketHeaders};
use rand::{
    distributions::{Alphanumeric, DistString},
    Rng,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, timeout},
};

use crate::{
    core::{
        context::{BluefinHost, Context, State},
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
        packet::{BluefinPacket, Packet},
        serialisable::Serialisable,
    },
    io::{
        manager::{ConnectionManager, Result},
        read::Read,
    },
};

const MAX_NUM_OPEN_STREAMS: usize = 10;
const MAX_NUMBER_OF_RETRIES: usize = 3;

/// A Bluefin `Connection`. This struct represents an established Bluefin connection
/// and keeps track of the state of the connection, input/output buffers and other
/// misc. Bluefin connection contexts.
///
/// Each `Connection` is unique and is identified by the `id` field.
#[derive(Debug)]
pub struct Connection {
    pub id: String,
    pub(crate) need_ip_udp_headers: bool,
    pub(crate) file: Arc<Mutex<File>>,
    pub source_id: u32,
    pub dest_id: u32,
    pub num_streams: usize,
    timeout: Duration,
    pub(crate) bytes_out: Option<Vec<u8>>,
    pub(crate) source_ip: Option<[u8; 4]>,
    pub(crate) destination_ip: Option<[u8; 4]>,
    pub(crate) source_port: Option<u16>,
    pub(crate) destination_port: Option<u16>,
    pub(crate) context: Context,
    pub(crate) conn_manager: Arc<Mutex<ConnectionManager>>,
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
            "BluefinConnection<{}> ({:?})
            src_id: {:#08x}
            dst_id: {:#08x}",
            self.id, self.context.state, self.source_id, self.dest_id
        )
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        eprintln!("Dropping connection {}", self.id);
    }
}

impl Connection {
    /// Returns a new connection instance.
    /// TODO: we might need to be stricter with this and return a Result<> instead
    pub(crate) fn new(
        dest_id: u32,
        packet_number: i64,
        source_ip: [u8; 4],
        source_port: u16,
        destination_ip: [u8; 4],
        destination_port: u16,
        host_type: BluefinHost,
        need_ip_udp_headers: bool,
        file: Arc<Mutex<File>>,
        conn_manager: Arc<Mutex<ConnectionManager>>,
    ) -> Connection {
        let source_id: u32 = rand::thread_rng().gen();
        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        Connection {
            id,
            need_ip_udp_headers,
            file,
            source_id,
            dest_id,
            num_streams: 0,
            timeout: Duration::from_secs(10),
            bytes_out: None,
            source_ip: Some(source_ip),
            destination_ip: Some(destination_ip),
            source_port: Some(source_port),
            destination_port: Some(destination_port),
            conn_manager,
            context: Context {
                host_type,
                state: State::Handshake,
                packet_number,
            },
        }
    }

    /*
    pub async fn request_stream(&mut self) -> Result<Stream, BluefinError> {
        // Can't request a stream if the connection is not ready
        if self.context.state != State::Ready {
            return Err(BluefinError::CannotOpenStreamError);
        }

        // Can't open too many streams
        if self.num_streams >= MAX_NUM_OPEN_STREAMS {
            return Err(BluefinError::CannotOpenStreamError);
        }

        // Ok, let's build the stream packet (no payload)
        let packet = self.get_packet(None);
        let mut rng = rand::thread_rng();
        let stream_id: u32 = rng.gen();

        let stream_header = BluefinStreamHeader::new(stream_id, StreamPacketType::OpenRequest);
        let stream_packet = BluefinStreamPacket::builder()
            .header(packet.header)
            .stream_header(stream_header)
            .build();

        self.set_bytes_out(stream_packet.serialise());
        eprintln!("Writing... {:?}", self.bytes_out.clone().unwrap());
        self.write().await;

        let stream = Stream::new(
            stream_id,
            self.source_id,
            self.dest_id,
            self.raw_file.try_clone().await.unwrap(),
            self.need_ip_udp_headers,
            self.timeout,
        );

        Ok(stream)
    }
    pub async fn accept_stream(&mut self) -> Result<Stream, BluefinError> {
        if self.context.state != State::Ready {
            return Err(BluefinError::CannotOpenStreamError);
        }

        if self.num_streams >= MAX_NUM_OPEN_STREAMS {
            return Err(BluefinError::CannotOpenStreamError);
        }

        let mut buf = vec![0; 1504];
        let (offset, size) = self.read(&mut buf).await.unwrap();
        eprintln!("Read stream request: {:?}", &buf[offset..offset + size]);

        let id: u32 = 0;
        let stream = Stream::new(
            id,
            self.source_id,
            self.dest_id,
            self.raw_file.try_clone().await.unwrap(),
            self.need_ip_udp_headers,
            self.timeout,
        );
        self.num_streams += 1;
        Ok(stream)
    }
    */

    pub(crate) fn set_bytes_out(&mut self, bytes_out: Vec<u8>) {
        self.bytes_out = Some(bytes_out);
    }

    pub(crate) fn need_ip_udp_headers(&mut self, need_ip_udp_headers: bool) {
        self.need_ip_udp_headers = need_ip_udp_headers;
    }

    /// This function is similar to `read` except:
    ///     1. Return value is wrapped with a `Result<BluefinReadResult, BluefinError>` instead of
    ///        io::Result<>. This makes it easier to deal with bluefin-specific errors.
    ///     2. This function is expecting the input to be a bluefin packet and not arbitrary bytes. If
    ///        the incoming bytes can not be deserialised into a bluefin packet then this errors.
    ///     3. No need to provide a buffer; we internally malloc 1504 byte buffer. This means the invoker
    ///        does not necessarily need to work with buffer ptr arithmetic.
    pub async fn bluefin_read_packet(&mut self) -> Result<BluefinReadResult> {
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

    async fn read_impl(
        &mut self,
        key: Option<String>,
        timeout: Option<Duration>,
    ) -> Result<Packet> {
        let key = key.unwrap_or(format!("{}_{}", self.dest_id, self.source_id));
        let timeout = timeout.unwrap_or(Duration::from_secs(3));
        let read = Read::new(
            key,
            Arc::clone(&self.file),
            Arc::clone(&self.conn_manager),
            self.need_ip_udp_headers,
        );
        let mut num_retries = 0;

        while num_retries < MAX_NUMBER_OF_RETRIES {
            if let Ok(packet) = tokio::time::timeout(timeout, read.clone()).await {
                return Ok(packet);
            }

            let jitter: u64 = rand::thread_rng().gen_range(0..=3);
            let sleep_in_secs = 2_u64.pow(num_retries as u32) + jitter;
            sleep(Duration::from_secs(sleep_in_secs)).await;

            num_retries += 1;
        }

        eprintln!("Timed out read!");
        Err(BluefinError::ReadError(
            "Failed to read a packet.".to_string(),
        ))
    }

    pub(crate) async fn read_with_id(&mut self, id: String) -> Result<Packet> {
        self.read_impl(Some(id), None).await
    }

    pub(crate) async fn read_with_timeout(&mut self, duration: Duration) -> Result<Packet> {
        self.read_impl(None, Some(duration)).await
    }

    pub(crate) async fn read_with_id_and_timeout(
        &mut self,
        id: String,
        duration: Duration,
    ) -> Result<Packet> {
        self.read_impl(Some(id), Some(duration)).await
    }

    /// Default read future. This reads any buffered content at the default id `<other>_<this>` and
    /// times out after 3 seconds.
    pub(crate) async fn read_v2(&mut self) -> Result<Packet> {
        self.read_impl(None, None).await
    }

    /// read has a built in timeout of 10 sec. If the future does not yield a
    /// result in thie timeout period, then this function returns an error.
    /// Otherwise, the exact read contents are stored in the conn.bytes_in.
    ///
    /// Returns an offset and size pair
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> io::Result<(usize, usize)> {
        let mut file = (*self.file).lock().unwrap();
        let mut size = timeout(self.timeout, file.read(buf)).await??;
        drop(file);

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

    pub(crate) async fn write_error_message(&mut self, err_msg: &[u8]) -> io::Result<()> {
        // TODO: I don't know why I need to try_clone. Without this, the write() invocations
        // is blocked and never completes.
        let mut packet = self.get_packet(Some(err_msg.into()));
        packet.header.type_and_type_specific_payload.packet_type = PacketType::Error;
        self.context.state = State::Error;

        self.set_bytes_out(packet.serialise());
        self.write().await?;

        Ok(())
    }

    pub(crate) async fn write(&mut self) -> io::Result<()> {
        if self.bytes_out.is_none() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Can't write zero bytes",
            ));
        }

        let bytes_out = self.bytes_out.as_ref().unwrap();

        let mut file = (*self.file).lock().unwrap();

        // No need to pre-build; just flush buffer
        if !self.need_ip_udp_headers {
            let ans = file.write_all(bytes_out).await;
            return ans;
        }

        let packet_builder =
            PacketBuilder::ipv4(self.source_ip.unwrap(), self.destination_ip.unwrap(), 20)
                .udp(self.source_port.unwrap(), self.destination_port.unwrap());
        eprintln!(
            "dst: {:?}:{}",
            self.destination_ip.unwrap(),
            self.destination_port.unwrap()
        );

        let mut tun_tap_bytes = vec![0, 0, 0, 2];
        let mut writer = Vec::<u8>::with_capacity(packet_builder.size(bytes_out.capacity()));
        let _ = packet_builder.write(&mut writer, &bytes_out);
        tun_tap_bytes.append(&mut writer);

        self.bytes_out = None;
        timeout(Duration::from_secs(5), file.write_all(&tun_tap_bytes)).await?
    }

    pub(crate) fn get_packet(&self, payload: Option<Vec<u8>>) -> BluefinPacket {
        let packet_type = match self.context.state {
            State::Handshake => PacketType::UnencryptedHandshake,
            State::Error => PacketType::Error,
            State::DataStream => PacketType::Data,
            // We should never try to build an empty packet if the connection is closed
            State::Closed => unreachable!(),
            State::Ready => PacketType::Data,
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
}
