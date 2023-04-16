use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use rand::Rng;
use tokio::time::sleep;

use crate::core::packet::Packet;

use super::manager::ConnectionBuffer;

#[derive(Clone)]
pub(crate) struct BufferedRead {
    /// Unique id per buffered read request, mostly used for debugging purposes
    id: u32,
    /// This connection's buffer
    buffer: Arc<Mutex<ConnectionBuffer>>,
}

impl BufferedRead {
    pub(crate) fn new(buffer: Arc<Mutex<ConnectionBuffer>>) -> Self {
        let id: u32 = rand::thread_rng().gen();
        Self { id, buffer }
    }

    /// Consume the `BufferedRead` and attempt to read from the buffer. If no packet could be yielded then
    /// None is returned.
    pub(crate) async fn read(
        self,
        timeout: Option<Duration>,
        max_number_of_tries: Option<usize>,
    ) -> Option<Packet> {
        let timeout = timeout.unwrap_or(Duration::from_secs(3));
        let max_number_of_tries = max_number_of_tries.unwrap_or(3);
        let mut num_retries = 0;

        while num_retries < max_number_of_tries {
            if let Ok(packet) = tokio::time::timeout(timeout, self.clone()).await {
                return Some(packet);
            }
            num_retries += 1;

            if num_retries >= max_number_of_tries {
                break;
            }

            let jitter: u64 = rand::thread_rng().gen_range(0..=3);
            let sleep_in_millis = (2_u64.pow(num_retries as u32) + jitter) * 100;
            sleep(Duration::from_millis(sleep_in_millis)).await;
        }

        None
    }
}

impl Future for BufferedRead {
    type Output = Packet;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _self = self.as_ref();
        let mut guard = (*_self.buffer).lock().unwrap();

        if let Some(packet) = guard.consume() {
            // Reset waker
            guard.waker = None;
            return Poll::Ready(packet);
        }

        guard.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
