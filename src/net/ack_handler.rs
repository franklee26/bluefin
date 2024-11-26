use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use tokio::{sync::RwLock, time::sleep};

use crate::{
    core::{error::BluefinError, packet::BluefinPacket},
    utils::{
        common::BluefinResult,
        window::{SlidingWindow, SlidingWindowConsumeResult},
    },
};

#[derive(Clone)]
pub(crate) struct AckBuffer {
    received_acks: SlidingWindow,
    waker: Option<Waker>,
}

impl AckBuffer {
    pub(crate) fn new(smallest_expected_recv_packet_num: u64) -> Self {
        Self {
            received_acks: SlidingWindow::new(smallest_expected_recv_packet_num),
            waker: None,
        }
    }

    /// Buffers in the received ack
    pub(crate) fn buffer_in_ack_packet(&mut self, packet: BluefinPacket) -> BluefinResult<()> {
        let num_packets_to_ack = packet.header.type_specific_payload;
        let base_packet_num = packet.header.packet_number;
        for ix in 0..num_packets_to_ack {
            self.received_acks
                .insert_packet_number(base_packet_num + ix as u64)?;
        }
        Ok(())
    }

    #[inline]
    fn consume(&mut self) -> Option<SlidingWindowConsumeResult> {
        self.received_acks.consume()
    }

    #[inline]
    pub(crate) fn wake(&mut self) -> BluefinResult<()> {
        if let Some(ref waker) = self.waker {
            waker.wake_by_ref();
            return Ok(());
        }
        Err(BluefinError::NoSuchWakerError)
    }
}

#[derive(Clone)]
struct AckConsumerFuture {
    ack_buff: Arc<Mutex<AckBuffer>>,
}

impl Future for AckConsumerFuture {
    type Output = SlidingWindowConsumeResult;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut guard = self.ack_buff.lock().unwrap();
        if let Some(res) = guard.consume() {
            return Poll::Ready(res);
        }
        guard.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[derive(Clone)]
pub(crate) struct AckConsumer {
    future: AckConsumerFuture,
    largest_recv_acked_packet_num: Arc<RwLock<u64>>,
}

impl AckConsumer {
    pub(crate) fn new(
        ack_buff: Arc<Mutex<AckBuffer>>,
        largest_recv_acked_packet_num: Arc<RwLock<u64>>,
    ) -> Self {
        let future = AckConsumerFuture { ack_buff };
        Self {
            future,
            largest_recv_acked_packet_num,
        }
    }

    pub(crate) async fn run(&self) {
        loop {
            let res = self.future.clone().await;

            {
                let mut guard = self.largest_recv_acked_packet_num.write().await;
                *guard = res.largest_packet_number;
            }

            sleep(Duration::from_micros(5)).await;
        }
    }
}
