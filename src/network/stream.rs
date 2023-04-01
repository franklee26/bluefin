use std::time::Duration;

use etherparse::{PacketBuilder, PacketHeaders};
use rand::distributions::{Alphanumeric, DistString};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

use crate::core::{error::BluefinError, packet::BluefinPacket, serialisable::Serialisable};

pub struct Stream {
    pub id: u32,
    pub(crate) src_id: i32,
    pub(crate) dst_id: i32,
    pub(crate) need_ip_udp_headers: bool,
    raw_file: File,
    timeout: Duration,
}

pub struct Receive {
    pub size: usize,
    pub offset: usize,
    pub packet: BluefinPacket,
}

impl Stream {
    pub(crate) fn new(
        id: u32,
        src_id: i32,
        dst_id: i32,
        raw_file: File,
        need_ip_udp_headers: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            id,
            src_id,
            dst_id,
            need_ip_udp_headers,
            raw_file,
            timeout,
        }
    }

    pub async fn receive(&mut self) -> Result<Receive, BluefinError> {
        let mut buf = vec![0; 1504];
        match timeout(self.timeout, self.raw_file.read(&mut buf)).await {
            Ok(timeout_result) => match timeout_result {
                Ok(mut size) => {
                    // If I don't need to worry about ip/udp headers then I'm using a UDP socket. This means
                    // that I'm just getting the Bluefin packet. Easy.
                    if !self.need_ip_udp_headers {
                        let packet = BluefinPacket::deserialise(&buf)?;
                        return Ok(Receive {
                            size,
                            offset: 0,
                            packet,
                        });
                    }

                    // Else, I need to worry about the additional ip + udp headers. Let's strip them and
                    // return.
                    let ip_packet = PacketHeaders::from_ip_slice(&buf[4..]).unwrap();
                    let ip_header_len = ip_packet.ip.unwrap().header_len();
                    let udp_header_len = ip_packet.transport.unwrap().udp().unwrap().header_len();

                    let offset = 4 + ip_header_len + udp_header_len;
                    let packet = BluefinPacket::deserialise(&buf[offset..])?;
                    size -= offset;

                    Ok(Receive {
                        size,
                        offset,
                        packet,
                    })
                }
                Err(e) => Err(BluefinError::ReadError(e.to_string())),
            },
            Err(e) => Err(BluefinError::ReadError(e.to_string())),
        }
    }
}
