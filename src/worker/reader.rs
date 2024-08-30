use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use tokio::{net::UdpSocket, sync::RwLock, time::sleep};

use crate::{
    core::{context::BluefinHost, header::PacketType, packet::BluefinPacket, Serialisable},
    net::{
        connection::{ConnectionBuffer, ConnectionManager},
        is_client_ack_packet, is_hello_packet,
    },
    utils::common::BluefinResult,
};

#[derive(Clone)]
pub(crate) struct TxChannel {
    pub(crate) id: u8,
    socket: Arc<UdpSocket>,
    conn_manager: Arc<RwLock<ConnectionManager>>,
    pending_accept_ids: Arc<Mutex<Vec<u32>>>,
    host_type: BluefinHost,
}

#[derive(Clone)]
pub(crate) struct RxChannel {
    conn_id: u32,
    buffer: Arc<Mutex<ConnectionBuffer>>,
}

impl Future for RxChannel {
    type Output = (Vec<u8>, SocketAddr);

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.buffer.lock().unwrap();
        if let Ok((bytes, addr)) = guard.consume() {
            return Poll::Ready((bytes, addr));
        }

        guard.set_waker(cx.waker().clone());
        Poll::Pending
    }
}

impl RxChannel {
    pub(crate) fn new(conn_id: u32, buffer: Arc<Mutex<ConnectionBuffer>>) -> Self {
        Self { conn_id, buffer }
    }

    pub(crate) async fn read(&self) -> (Vec<u8>, SocketAddr) {
        self.clone().await
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
        if *encountered_err {
            sleep(Duration::from_millis(100)).await;
            *encountered_err = false;
        }
    }

    pub(crate) async fn run(&mut self) -> BluefinResult<()> {
        let mut encountered_err = false;

        loop {
            TxChannel::run_sleep(&mut encountered_err).await;

            let mut buf = vec![0; 1504];

            let (res, addr) = self.socket.recv_from(&mut buf).await?;

            let packet_res = BluefinPacket::deserialise(&buf[..res]);

            // Not a bluefin packet or it's invalid. Log and retry.
            if let Err(e) = packet_res {
                eprintln!("{}", e);
                encountered_err = true;
                continue;
            }

            // Acquire lock and buffer in data
            let mut packet = packet_res.unwrap();
            let mut src_conn_id = packet.header.destination_connection_id;
            let dst_conn_id = packet.header.source_connection_id;
            let mut is_hello = false;
            let mut is_client_ack = false;

            {
                if is_hello_packet(self.host_type, &packet) {
                    match self.host_type {
                        BluefinHost::PackLeader => {
                            // Choose a conn id to buffer this in FIFO
                            if let Some(id) = self.pending_accept_ids.lock().unwrap().pop() {
                                src_conn_id = id;
                                is_hello = true;
                            } else {
                                eprintln!("No pending accepts ready!");
                                encountered_err = true;
                                continue;
                            }
                        }
                        BluefinHost::Client => {
                            // nothing to do!
                            is_hello = true;
                        }
                        _ => {
                            unimplemented!();
                        }
                    }
                }

                if is_client_ack_packet(self.host_type, &packet) {
                    is_client_ack = true;
                }

                let guard = self.conn_manager.read().await;
                let key = {
                    if is_hello {
                        format!("{}_0", src_conn_id)
                    } else {
                        format!("{}_{}", src_conn_id, dst_conn_id)
                    }
                };
                let _conn_buf = guard.get(&key);
                drop(guard);

                if _conn_buf.is_none() {
                    eprintln!("Could not find connection {}", &key);
                    encountered_err = true;
                    continue;
                }

                let buffer = _conn_buf.unwrap();

                let mut buffer_guard = buffer.lock().unwrap();
                // If not hello, we buffer in the bytes
                if !is_hello && !is_client_ack {
                    let buffer_res = buffer_guard.buffer_in_bytes(&mut packet);

                    // Could not buffer in packet... buffer is likely full. We will have to discard the
                    // packet.
                    if let Err(e) = buffer_res {
                        eprintln!("{:?}", e);
                        encountered_err = true;
                        continue;
                    }
                } else {
                    let buffer_res = buffer_guard.buffer_in_packet(packet.clone());
                    if let Err(e) = buffer_res {
                        eprintln!("{:?}", e);
                        encountered_err = true;
                        continue;
                    }
                    if let Err(_) = buffer_guard.buffer_in_addr(addr) {
                        // encountered_err = true;
                        // continue;
                    }
                }

                buffer_guard.set_dst_conn_id(packet.header.source_connection_id);

                // Wake future that buffered data is available
                if let Some(w) = buffer_guard.get_waker() {
                    w.wake_by_ref();
                }
            }
        }
    }
}
