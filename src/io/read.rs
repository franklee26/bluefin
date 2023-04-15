use std::{sync::Arc, time::Duration};

use etherparse::{Ipv4Header, PacketHeaders};
use rand::distributions::{Alphanumeric, DistString};
use tokio::{fs::File, io::AsyncReadExt, time::sleep};

use crate::core::context::BluefinHost;
use crate::core::header::PacketType;
use crate::core::serialisable::Serialisable;
use crate::core::{error::BluefinError, packet::BluefinPacket, packet::Packet};

use super::manager::{ConnectionManager, Result};

/// Essentially a read-worker thread. Each worker attempts to read in bytes from the wire. Then,
/// the worker determiens if the packet is a valid bluefin packet. If so, the worker will attempt
/// to buffer the packet in the appropriate buffer.
pub(crate) struct ReadWorker {
    id: String,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    file: File,
    need_ip_and_udp_headers: bool,
    host_type: BluefinHost,
}

/// Worker thread that occasionally wakes up and peeks into any possible matches between outstanding
/// `Accept` requests and any outstanding buffered unhandled client hellos.
pub(crate) struct AcceptWorker {
    id: String,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    max_number_tries: usize,
}

impl AcceptWorker {
    pub(crate) fn new(manager: Arc<tokio::sync::Mutex<ConnectionManager>>) -> Self {
        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        // TODO: adjust the number more dynamically?
        let max_number_tries = 10;
        Self {
            id,
            manager,
            max_number_tries,
        }
    }

    /// Runs `max_number_of_tries` times. Note that each accept invocation should correspond with one
    /// worker cleaner runner in the background. In otherwords, why would we want to run this if we never
    /// made an accept request or if all of them have already been fulfilled? This means that if we
    /// were able to wake up an accept then our job is done and we can exit.
    pub(crate) async fn run(&self) {
        for _ in 0..self.max_number_tries {
            sleep(Duration::from_secs(1)).await;

            // Lock acquired
            let mut manager = self.manager.lock().await;

            match manager.try_to_wake_buffered_accept() {
                Ok(_) => {
                    return;
                }
                Err(_) => continue,
            }
        }
    }
}

impl ReadWorker {
    pub(crate) fn new(
        manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
        file: File,
        need_ip_and_udp_headers: bool,
        host_type: BluefinHost,
    ) -> Self {
        let id = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        Self {
            id,
            manager,
            file,
            need_ip_and_udp_headers,
            host_type,
        }
    }

    /// Helper to determine whether a given `packet` is a valid hello packet eg. client-hello or pack-leader-hello
    fn is_hello_packet(&self, packet: &Packet) -> bool {
        let header = packet.payload.header;
        let other_id = header.source_connection_id;
        let this_id = header.destination_connection_id;

        // if client-hello then, must have a non-zero source id
        if self.host_type == BluefinHost::PackLeader && other_id == 0x0 {
            return false;
        }

        // if client-hello then, the destination id must be 0x0
        if self.host_type == BluefinHost::PackLeader && this_id != 0x0 {
            return false;
        }

        // Client hellos' must have type UnencryptedHandshake
        if header.type_and_type_specific_payload.packet_type != PacketType::UnencryptedHandshake {
            return false;
        }

        true
    }

    fn get_packet_from_buffer(&self, buf: &[u8]) -> Result<Packet> {
        let mut offset = 0;
        let mut ip_header_len = 0;
        let mut udp_header_len = 0;
        let mut src_ip = [0; 4];
        let mut dst_ip = [0; 4];
        let mut src_port = 0;
        let mut dst_port = 0;

        // If I need to worry about ip and udp headers then I need to also worry about tun/tap bytes.
        // Offset the tun/tap header, which is 4 bytes
        if self.need_ip_and_udp_headers {
            offset = 4;
        }

        if let Ok(packet_headers) = PacketHeaders::from_ip_slice(&buf[offset..]) {
            ip_header_len = packet_headers.ip.unwrap().header_len();
            let (ip_header, _) =
                Ipv4Header::from_slice(&buf[offset..ip_header_len + offset]).unwrap();

            // Not udp; drop.
            if ip_header.protocol != 0x11 {
                // TODO: Return better error
                return Err(BluefinError::DeserialiseError("Not UDP-based.".to_string()));
            }

            let udp_header = packet_headers.transport.unwrap().udp().unwrap();
            udp_header_len = udp_header.header_len();

            src_ip = ip_header.source;
            dst_ip = ip_header.destination;
            src_port = udp_header.source_port;
            dst_port = udp_header.destination_port;
        }

        let wrapped_bluefin_packet =
            BluefinPacket::deserialise(&buf[offset + ip_header_len + udp_header_len..]);

        // Not a bluefin packet; drop.
        if let Err(_) = wrapped_bluefin_packet {
            // TODO: Return better error
            return Err(BluefinError::DeserialiseError(
                "Wrong connection".to_string(),
            ));
        }

        let payload = wrapped_bluefin_packet.unwrap();
        Ok(Packet {
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            payload,
        })
    }

    /// Run a single, infinite loop. This continuously reads from the network, verifies if it is an Bluefin packet,
    /// performs simple validations and tries to buffer the packet via the `ConnectionManager`.
    pub(crate) async fn run(&mut self) {
        loop {
            sleep(Duration::from_secs(3)).await;

            let mut buf = vec![0; 1504];
            let res = self.file.read(&mut buf).await;

            // Can't work with a read error
            if let Err(_) = res {
                continue;
            }

            let size = res.unwrap();
            let packet_res = self.get_packet_from_buffer(&buf[..size]);

            // Not a bluefin packet
            if let Err(_) = packet_res {
                continue;
            }

            let packet = packet_res.unwrap();
            let header = packet.payload.header;
            let key_from_packet = format!(
                "{}_{}",
                header.source_connection_id, header.destination_connection_id
            );
            let is_hello = self.is_hello_packet(&packet);

            // Lock acquired
            {
                let mut manager = self.manager.lock().await;

                match self.host_type {
                    BluefinHost::PackLeader => {
                        // Found a client hello AND I have never encountered this connection before.
                        // This must be an client-hello awaiting a pack-leader's `Accept`.
                        let buffer_exists = manager.buffer_exists(&key_from_packet);
                        if is_hello && !buffer_exists {
                            let _ = manager.buffer_client_hello(packet);
                            continue;
                        }
                    }
                    BluefinHost::Client => {
                        let potential_pack_leader_key =
                            format!("0_{}", header.destination_connection_id);
                        let open_buffer_exists = manager.buffer_exists(&potential_pack_leader_key);

                        // Found a pack-leader hello AND I have an open connection awaiting a response
                        if is_hello && open_buffer_exists {
                            let _ = manager.buffer_packet_to_existing_connection(
                                &potential_pack_leader_key,
                                packet,
                            );
                            continue;
                        }
                    }
                    _ => todo!(),
                }

                // Let's try buffering this to a connection, if it exists
                let _ = manager.buffer_packet_to_existing_connection(&key_from_packet, packet);
            }
            // Lock released
        }
    }
}
