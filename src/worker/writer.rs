use std::{
    collections::VecDeque,
    fmt::Write,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};

use tokio::net::UdpSocket;

use crate::{
    core::{packet::BluefinPacket, Serialisable},
    utils::common::BluefinResult,
};

pub(crate) struct WriterQueue {
    queue: VecDeque<BluefinPacket>,
    waker: Option<Waker>,
}

impl WriterQueue {
    pub(crate) fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            waker: None,
        }
    }
}

/// Queues write requests to be sent
pub(crate) struct WriterTxChannel {
    queue: Arc<Mutex<WriterQueue>>,
}

impl WriterTxChannel {
    pub(crate) fn new(queue: Arc<Mutex<WriterQueue>>) -> Self {
        Self { queue }
    }

    pub(crate) async fn send(&mut self, packet: BluefinPacket) -> BluefinResult<usize> {
        let bytes = packet.len();
        let mut guard = self.queue.lock().unwrap();
        guard.queue.push_back(packet);

        // Signal to Rx channel that we have new packets in the queue
        if let Some(ref waker) = guard.waker {
            waker.wake_by_ref();
        }
        Ok(bytes)
    }
}

/// Consumes queued requests and sends them across the wire
#[derive(Clone)]
pub(crate) struct WriterRxChannel {
    socket: Arc<UdpSocket>,
    queue: Arc<Mutex<WriterQueue>>,
    dst_addr: SocketAddr,
}

impl Future for WriterRxChannel {
    type Output = usize;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.queue.lock().unwrap();
        let num_packets_to_send = guard.queue.len();
        if num_packets_to_send == 0 {
            guard.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(num_packets_to_send)
    }
}

impl WriterRxChannel {
    pub(crate) fn new(
        queue: Arc<Mutex<WriterQueue>>,
        socket: Arc<UdpSocket>,
        dst_addr: SocketAddr,
    ) -> Self {
        Self {
            queue,
            socket,
            dst_addr,
        }
    }

    pub(crate) async fn run(&self) {
        loop {
            let mut num_packets_to_send = self.clone().await;
            let mut guard = self.queue.lock().unwrap();
            while num_packets_to_send > 0 && !guard.queue.is_empty() {
                let packet = guard.queue.pop_front().unwrap();
                if let Err(e) = self.socket.try_send_to(&packet.serialise(), self.dst_addr) {
                    eprintln!("Encountered error {} while sending packet across wire", e);
                    guard.queue.push_front(packet);
                    break;
                }
                num_packets_to_send -= 1;
            }
            guard.waker = None;
        }
    }
}
