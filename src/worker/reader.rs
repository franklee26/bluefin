use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use tokio::{net::UdpSocket, sync::RwLock, time::sleep};

use crate::{
    core::{context::BluefinHost, error::BluefinError, header::PacketType, packet::BluefinPacket},
    net::{
        ack_handler::AckBuffer,
        connection::{ConnectionBuffer, ConnectionManager},
        is_client_ack_packet, is_hello_packet, ConnectionManagedBuffers,
        MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM,
    },
    utils::common::BluefinResult,
};

use super::writer::WriterTxChannel;

#[derive(Clone)]
/// [ReaderTxChannel] is the transmission channel for the receiving [ReaderRxChannel]. This channel will when
/// [run](Self::run), asynchronously read from the udp socket and upon receiving a packet, the channel
/// attempts to serialise it to a bluefin packet. If a bluefin packet is found then the channel will
/// use the [conn_manager](Self::conn_manager) to identify the correct connection buffer and attempt
/// to buffer in the bytes/packet. In other words, this channel *transmits* bytes *into* the buffer
/// and signals any awaiters that data is ready.
pub(crate) struct ReaderTxChannel {
    pub(crate) id: u16,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    host_type: BluefinHost,
}

#[derive(Clone)]
/// [ReaderRxChannel] is the receiving channel for the transmitting [ReaderRxChannel]. This channel will when
/// [read](Self::read), asynchronously peek into [Self::buffer] and will eventually return the
/// buffered tuple contents ([ConsumeResult], [SocketAddr]). In other words, this channel
/// *receives* bytes *from* the buffer.
pub(crate) struct ReaderRxChannel {
    future: ReaderRxChannelFuture,
    writer_tx_channel: WriterTxChannel,
}

#[derive(Clone)]
struct ReaderRxChannelFuture {
    buffer: Arc<Mutex<ConnectionBuffer>>,
}

impl Future for ReaderRxChannelFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.buffer.lock().unwrap();
        if let Ok(()) = guard.peek() {
            return Poll::Ready(());
        }

        guard.set_waker(cx.waker().clone());
        Poll::Pending
    }
}

impl ReaderRxChannel {
    pub(crate) fn new(
        buffer: Arc<Mutex<ConnectionBuffer>>,
        writer_tx_channel: WriterTxChannel,
    ) -> Self {
        let future = ReaderRxChannelFuture { buffer };
        Self {
            future,
            writer_tx_channel,
        }
    }

    #[inline]
    pub(crate) async fn read(
        &mut self,
        bytes_to_read: usize,
        buf: &mut [u8],
    ) -> BluefinResult<(u64, SocketAddr)> {
        let _ = self.future.clone().await;
        let (consume_res, addr) = {
            let mut guard = self.future.buffer.lock().unwrap();
            guard.consume(bytes_to_read, buf).unwrap()
        };
        let num_packets_consumed = consume_res.get_num_packets_consumed();
        let base_packet_num = consume_res.get_base_packet_number();

        // We need to send an ack.
        if num_packets_consumed > 0 {
            if let Err(e) = self
                .writer_tx_channel
                .send_ack(base_packet_num, num_packets_consumed)
                .await
            {
                eprintln!(
                    "Failed to send ack packet after reads due to error: {:?}",
                    e
                );
            }
        }

        Ok((consume_res.get_bytes_consumed(), addr))
    }
}

impl ReaderTxChannel {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        conn_manager: Arc<RwLock<ConnectionManager>>,
        pending_accept_ids: Arc<Mutex<Vec<u32>>>,
        host_type: BluefinHost,
    ) -> Self {
        Self {
            id: 0,
            socket,
            conn_manager,
            pending_accept_ids,
            host_type,
        }
    }

    #[inline]
    async fn run_sleep(encountered_err: &mut bool) {
        if !*encountered_err {
            sleep(Duration::from_micros(1)).await;
            return;
        }
        sleep(Duration::from_millis(5)).await;
        *encountered_err = false;
    }

    #[inline]
    fn handle_for_handshake(
        &self,
        packet: &BluefinPacket,
        is_hello: &mut bool,
        src_conn_id: &mut u32,
    ) -> BluefinResult<()> {
        if is_hello_packet(self.host_type, &packet) {
            match self.host_type {
                BluefinHost::PackLeader => {
                    // Choose a conn id to buffer this in FIFO
                    if let Some(id) = self.pending_accept_ids.lock().unwrap().pop() {
                        *src_conn_id = id;
                        *is_hello = true;
                        return Ok(());
                    } else {
                        *is_hello = false;
                        return Err(BluefinError::CouldNotAcceptConnectionError(
                            "No pending accepts ready".to_string(),
                        ));
                    }
                }
                BluefinHost::Client => {
                    *is_hello = true;
                    return Ok(());
                }
                _ => {
                    unimplemented!();
                }
            }
        }

        *is_hello = false;
        Ok(())
    }

    #[inline]
    fn build_conn_buff_key(is_hello: bool, src_conn_id: u32, dst_conn_id: u32) -> String {
        return {
            if !is_hello {
                format!("{}_{}", src_conn_id, dst_conn_id)
            } else {
                format!("{}_0", src_conn_id)
            }
        };
    }

    fn buffer_to_conn_buffer(
        conn_buff: &mut ConnectionBuffer,
        packet: BluefinPacket,
        addr: SocketAddr,
        is_hello: bool,
        is_client_ack: bool,
    ) -> BluefinResult<()> {
        let packet_src_conn_id = packet.header.source_connection_id;
        if !is_hello && !is_client_ack {
            // If not hello, we buffer in the bytes
            // Could not buffer in packet... buffer is likely full. We will have to discard the
            // packet.
            conn_buff.buffer_in_bytes(packet)?;
        } else {
            conn_buff.buffer_in_packet(packet)?;
            let _ = conn_buff.buffer_in_addr(addr);
        }

        conn_buff.set_dst_conn_id(packet_src_conn_id);

        // Wake future that buffered data is available
        if let Some(w) = conn_buff.get_waker() {
            w.wake_by_ref();
        } else {
            return Err(BluefinError::NoSuchWakerError);
        }
        Ok(())
    }

    #[inline]
    fn buffer_to_ack_buffer(ack_buff: &mut AckBuffer, packet: BluefinPacket) -> BluefinResult<()> {
        ack_buff.buffer_in_ack_packet(packet)?;
        ack_buff.wake()
    }

    #[inline]
    fn buffer_in_data(
        is_hello: bool,
        host_type: BluefinHost,
        packet: BluefinPacket,
        addr: SocketAddr,
        buffers: &ConnectionManagedBuffers,
    ) -> BluefinResult<()> {
        let is_client_ack = is_client_ack_packet(host_type, &packet);
        if !is_client_ack && !is_hello && packet.header.type_field == PacketType::Ack {
            let mut ack_buff = buffers.ack_buff.lock().unwrap();
            Self::buffer_to_ack_buffer(&mut ack_buff, packet)?;
            drop(ack_buff);
        } else {
            let mut conn_buff = buffers.conn_buff.lock().unwrap();
            Self::buffer_to_conn_buffer(&mut conn_buff, packet, addr, is_hello, is_client_ack)?;
            drop(conn_buff);
        }
        Ok(())
    }

    /// The [TxChannel]'s engine runner. This method will run forever and is responsible for reading bytes
    /// from the udp socket into a connection buffer. This method should be run its own asynchronous task.
    pub(crate) async fn run(&mut self) -> BluefinResult<()> {
        let mut encountered_err = false;
        let mut buf = [0u8; MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM];

        loop {
            ReaderTxChannel::run_sleep(&mut encountered_err).await;

            let (size, addr) = self.socket.recv_from(&mut buf).await?;
            let packets_res = BluefinPacket::from_bytes(&buf[..size]);

            // Not bluefin packet(s) or it's invalid.
            if let Err(e) = packets_res {
                encountered_err = true;
                eprintln!("Encountered err: {:?}", e);
                continue;
            }

            // Acquire lock and buffer in data
            let packets = packets_res.unwrap();
            if packets.len() == 0 {
                encountered_err = true;
                continue;
            }

            // Because all bluefin packets bundled in a datagram must come from the same host, we just peek
            // at the first one
            let mut src_conn_id = packets[0].header.destination_connection_id;
            let dst_conn_id = packets[0].header.source_connection_id;
            let mut is_hello = false;

            // If there is only one packet, then it's possible it is a handshake packet. Handshakes are sent
            // via one udp datagram carries exactly one bluefin packet
            if packets.len() == 1 {
                if let Err(e) =
                    self.handle_for_handshake(&packets[0], &mut is_hello, &mut src_conn_id)
                {
                    eprintln!("{}", e);
                    encountered_err = true;
                    continue;
                }
            }

            let key = ReaderTxChannel::build_conn_buff_key(is_hello, src_conn_id, dst_conn_id);
            let _conn_buf = {
                // ACQUIRE LOCK FOR CONN MANAGER
                let guard = self.conn_manager.read().await;
                guard.get(&key)
                // We just need the conn buffer, which is behind its own lock. We don't need the
                // conn manager anymore.
                // RELEASE LOCK FOR CONN MANAGER
            };

            if _conn_buf.is_none() {
                eprintln!("Could not find connection {}", &key);
                encountered_err = true;
                continue;
            }

            let buffers = _conn_buf.unwrap();
            for p in packets {
                if let Err(e) =
                    ReaderTxChannel::buffer_in_data(is_hello, self.host_type, p, addr, &buffers)
                {
                    eprintln!("Failed to buffer in data: {}", e);
                    encountered_err = true;
                }
            }
        }
    }
}
