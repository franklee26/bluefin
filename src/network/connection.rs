use std::{
    fmt::{self, Debug},
    sync::Arc,
    time::Duration,
};

use etherparse::PacketBuilder;
use rand::{
    distributions::{Alphanumeric, DistString},
    Rng,
};
use tokio::{fs::File, io::AsyncWriteExt, time::timeout};

use crate::{
    core::{
        context::{BluefinHost, Context, State},
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::{BluefinPacket, Packet},
    },
    io::{
        buffered_read::BufferedRead, manager::ConnectionBuffer, stream_manager::StreamManager,
        Result,
    },
};

use super::stream::Stream;

const MAX_NUM_OPEN_STREAMS: usize = 10;

/// A Bluefin `Connection`. This struct represents an established Bluefin connection
/// and keeps track of the state of the connection, input/output buffers and other
/// misc. Bluefin connection contexts.
///
/// Each `Connection` is unique and is identified by the `id` field.
#[derive(Debug)]
pub struct Connection {
    pub id: String,
    pub(crate) need_ip_udp_headers: bool,
    pub(crate) file: File,
    pub source_id: u32,
    pub dest_id: u32,
    pub num_streams: usize,
    pub(crate) source_ip: Option<[u8; 4]>,
    pub(crate) destination_ip: Option<[u8; 4]>,
    pub(crate) source_port: Option<u16>,
    pub(crate) destination_port: Option<u16>,
    pub(crate) context: Context,
    pub(crate) buffer: Arc<std::sync::Mutex<ConnectionBuffer>>,
    pub(crate) stream_manager: Arc<tokio::sync::Mutex<StreamManager>>,
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

impl Connection {
    /// Returns a new connection instance.
    /// TODO: we might need to be stricter with this and return a Result<> instead
    pub(crate) fn new(
        dest_id: u32,
        packet_number: u64,
        source_ip: [u8; 4],
        source_port: u16,
        destination_ip: [u8; 4],
        destination_port: u16,
        host_type: BluefinHost,
        need_ip_udp_headers: bool,
        file: File,
        buffer: Arc<std::sync::Mutex<ConnectionBuffer>>,
        stream_manager: Arc<tokio::sync::Mutex<StreamManager>>,
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
            source_ip: Some(source_ip),
            destination_ip: Some(destination_ip),
            source_port: Some(source_port),
            destination_port: Some(destination_port),
            buffer,
            stream_manager,
            context: Context {
                host_type,
                state: State::Handshake,
                packet_number,
            },
        }
    }

    pub async fn request_stream(&mut self) -> Result<Stream> {
        todo!()
    }

    pub async fn accept_stream(&mut self) -> Result<Stream> {
        todo!()
    }

    pub(crate) fn need_ip_udp_headers(&mut self, need_ip_udp_headers: bool) {
        self.need_ip_udp_headers = need_ip_udp_headers;
    }

    async fn reset_file(&mut self) {
        self.file = self.file.try_clone().await.unwrap();
    }

    /// Identical to `read` but with a specified timeout `duration`
    pub async fn read_with_timeout(&mut self, duration: Duration) -> Result<Packet> {
        self.read_impl(Some(duration), None).await
    }

    /// Identical to `read` but with a specified timeout `duration` and an `max_number_of_tries` retries
    pub async fn read_with_timeout_and_retries(
        &mut self,
        duration: Duration,
        max_number_of_tries: usize,
    ) -> Result<Packet> {
        self.read_impl(Some(duration), Some(max_number_of_tries))
            .await
    }

    /// Default `Connection`-based read (as opposed to stream reads). This tries to read from the
    /// connection buffer with a default timeout duration of 3 seconds and a max of two total
    /// tries. Note that this read should only really be used for handshake and error communications
    /// between hosts. While data transfer is allowed with direct connections, it is recommended
    /// to use `Stream`-based reads instead.
    ///
    /// Note that this invocation is blocking as it accesses the `connection_manager`, which is
    /// shared across all connection's per device. This is why `Stream` reads are recommended; they
    /// are less blocking than this as this can block the `accept()` process.
    pub async fn read(&mut self) -> Result<Packet> {
        self.read_impl(None, None).await
    }

    async fn read_impl(
        &mut self,
        timeout: Option<Duration>,
        max_number_of_tries: Option<usize>,
    ) -> Result<Packet> {
        let buffered_read = BufferedRead::new(Arc::clone(&self.buffer));
        if let Some(packet) = buffered_read.read(timeout, max_number_of_tries).await {
            return Ok(packet);
        }

        // Reset file
        self.reset_file().await;

        Err(BluefinError::ReadError(
            "Failed to read a packet.".to_string(),
        ))
    }

    /// `Connection`-based writes. Unlike `Connection`-reads, this call is not blocking. This writes in the
    /// `bytes` + any required udp + IP headers to the wire. The invoker will need to prepare the Bluefin headers.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        // No need to pre-build; just flush buffer
        if !self.need_ip_udp_headers {
            match self.file.write_all(bytes).await {
                Ok(_) => return Ok(()),
                Err(e) => return Err(BluefinError::WriteError(format!("Failed to write. {}", e))),
            }
        }

        let packet_builder =
            PacketBuilder::ipv4(self.source_ip.unwrap(), self.destination_ip.unwrap(), 20)
                .udp(self.source_port.unwrap(), self.destination_port.unwrap());

        let mut tun_tap_bytes = vec![0, 0, 0, 2];
        let mut writer = Vec::<u8>::with_capacity(packet_builder.size(bytes.len()));
        let _ = packet_builder.write(&mut writer, &bytes);
        tun_tap_bytes.append(&mut writer);

        let wrapped = timeout(Duration::from_secs(3), self.file.write_all(&tun_tap_bytes)).await;
        if let Err(e) = wrapped {
            return Err(BluefinError::WriteError(format!("Failed to write. {}", e)));
        }
        let timeout = wrapped.unwrap();

        match timeout {
            Ok(()) => Ok(()),
            Err(e) => Err(BluefinError::WriteError(format!("Failed to write. {}", e))),
        }
    }

    /// Helper to build a `BluefinPacket` along with an optional data `payload`.
    pub fn get_packet(&self, payload: Option<Vec<u8>>) -> BluefinPacket {
        let packet_type = match self.context.state {
            State::Handshake => PacketType::UnencryptedHandshake,
            State::Error => PacketType::Error,
            State::DataStream => PacketType::Data,
            // We should never try to build an empty packet if the connection is closed
            State::Closed => unreachable!(),
            State::Ready => PacketType::Data,
        };
        // TODO: additional type specific fields?
        // TODO: tls encryption + mask?
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            self.source_id,
            self.dest_id,
            packet_type,
            0x0,
            security_fields,
        );
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
