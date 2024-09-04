use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use tokio::{net::UdpSocket, spawn, sync::RwLock, time::sleep};

use crate::{
    core::{
        context::BluefinHost, error::BluefinError, header::PacketType, packet::BluefinPacket,
        Serialisable,
    },
    net::{
        build_ack_packet,
        connection::{ConnectionBuffer, ConnectionManager},
        is_client_ack_packet, is_hello_packet,
        ordered_bytes::ConsumeResult,
    },
    utils::common::BluefinResult,
};

#[derive(Clone)]
/// [TxChannel] is the transmission channel for the receiving [RxChannel]. This channel will when
/// [run](Self::run), asynchronously read from the udp socket and upon receiving a packet, the channel
/// attempts to serialise it to a bluefin packet. If a bluefin packet is found then the channel will
/// use the [conn_manager](Self::conn_manager) to identify the correct connection buffer and attempt
/// to buffer in the bytes/packet. In other words, this channel *transmits* bytes *into* the buffer
/// and signals any awaiters that data is ready.
pub(crate) struct TxChannel {
    pub(crate) id: u8,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    host_type: BluefinHost,
}

#[derive(Clone)]
/// [RxChannel] is the receiving channel for the transmitting [RxChannel]. This channel will when
/// [read](Self::read), asynchronously peek into [Self::buffer] and will eventually return the
/// buffered tuple contents ([ConsumeResult], [SocketAddr]). In other words, this channel
/// *receives* bytes *from* the buffer.
pub(crate) struct RxChannel {
    socket: Arc<UdpSocket>,
    buffer: Arc<Mutex<ConnectionBuffer>>,
    bytes_to_read: usize,
    src_conn_id: u32,
    dst_conn_id: u32,
}

impl Future for RxChannel {
    type Output = (ConsumeResult, SocketAddr);

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.buffer.lock().unwrap();
        if let Ok((consume_res, addr)) = guard.consume(self.bytes_to_read) {
            return Poll::Ready((consume_res, addr));
        }

        guard.set_waker(cx.waker().clone());
        Poll::Pending
    }
}

impl RxChannel {
    pub(crate) fn new(
        buffer: Arc<Mutex<ConnectionBuffer>>,
        socket: Arc<UdpSocket>,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Self {
        Self {
            buffer,
            socket,
            src_conn_id,
            dst_conn_id,
            bytes_to_read: 0,
        }
    }

    #[inline]
    pub(crate) fn set_bytes_to_read(&mut self, bytes_to_read: usize) {
        self.bytes_to_read = bytes_to_read;
    }

    #[inline]
    pub(crate) async fn read(&self) -> BluefinResult<(Vec<u8>, SocketAddr)> {
        let (consume_res, addr) = self.clone().await;
        let num_packets_consumed = consume_res.get_num_packets_consumed();
        let base_packet_num = consume_res.get_base_packet_number();

        // We need to send an ack.
        if num_packets_consumed > 0 {
            let ack_packet = build_ack_packet(
                self.src_conn_id,
                self.dst_conn_id,
                base_packet_num,
                num_packets_consumed as u16,
            );
            let s = Arc::clone(&self.socket);
            spawn(async move { s.send_to(&ack_packet.serialise(), addr).await });
        }

        Ok((consume_res.take_bytes(), addr))
    }
}

impl TxChannel {
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
            return;
        }
        sleep(Duration::from_millis(100)).await;
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
            if is_hello {
                format!("{}_0", src_conn_id)
            } else {
                format!("{}_{}", src_conn_id, dst_conn_id)
            }
        };
    }

    #[inline]
    fn buffer_in_data(
        is_hello: bool,
        is_client_ack: bool,
        packet: &BluefinPacket,
        addr: SocketAddr,
        buffer: Arc<Mutex<ConnectionBuffer>>,
    ) -> BluefinResult<()> {
        let mut buffer_guard = buffer.lock().unwrap();

        // Ack packets are buffered differently
        if !is_client_ack && !is_hello && packet.header.type_field == PacketType::Ack {
            if let Err(e) = buffer_guard.buffer_in_ack_packet(&packet) {
                return Err(e);
            }
        } else if !is_hello && !is_client_ack {
            // If not hello, we buffer in the bytes
            // Could not buffer in packet... buffer is likely full. We will have to discard the
            // packet.
            if let Err(e) = buffer_guard.buffer_in_bytes(packet) {
                return Err(e);
            }
        } else {
            if let Err(e) = buffer_guard.buffer_in_packet(&packet) {
                return Err(e);
            }
            let _ = buffer_guard.buffer_in_addr(addr);
        }

        buffer_guard.set_dst_conn_id(packet.header.source_connection_id);

        // Wake future that buffered data is available
        if let Some(w) = buffer_guard.get_waker() {
            w.wake_by_ref();
        } else {
            return Err(BluefinError::NoSuchWakerError);
        }

        Ok(())
    }

    /// The [TxChannel]'s engine runner. This method will run forever and is responsible for reading bytes
    /// from the udp socket into a connection buffer. This method should be run its own asynchronous task.
    pub(crate) async fn run(&mut self) -> BluefinResult<()> {
        let mut encountered_err = false;

        loop {
            TxChannel::run_sleep(&mut encountered_err).await;

            let mut buf = vec![0; 1504];
            let (res, addr) = self.socket.recv_from(&mut buf).await?;
            let packet_res = BluefinPacket::deserialise(&buf[..res]);

            // Not a bluefin packet or it's invalid.
            if let Err(e) = packet_res {
                eprintln!("{}", e);
                encountered_err = true;
                continue;
            }

            // Acquire lock and buffer in data
            let packet = packet_res.unwrap();
            let mut src_conn_id = packet.header.destination_connection_id;
            let dst_conn_id = packet.header.source_connection_id;
            let mut is_hello = false;
            let mut is_client_ack = false;

            if let Err(e) = self.handle_for_handshake(&packet, &mut is_hello, &mut src_conn_id) {
                eprintln!("{}", e);
                encountered_err = true;
                continue;
            }

            if is_client_ack_packet(self.host_type, &packet) {
                is_client_ack = true;
            }

            let key = TxChannel::build_conn_buff_key(is_hello, src_conn_id, dst_conn_id);
            // ACQUIRE LOCK FOR CONN MANAGER
            let guard = self.conn_manager.read().await;
            let _conn_buf = guard.get(&key);
            // We just need the conn buffer, which is behind its own lock. We don't need the
            // conn manager anymore.
            // RELEASE LOCK FOR CONN MANAGER
            drop(guard);

            if _conn_buf.is_none() {
                eprintln!("Could not find connection {}", &key);
                encountered_err = true;
                continue;
            }

            let buffer = _conn_buf.unwrap();
            if let Err(e) =
                TxChannel::buffer_in_data(is_hello, is_client_ack, &packet, addr, buffer)
            {
                eprintln!("Failed to buffer in data: {}", e);
                encountered_err = true;
            }
        }
    }
}
