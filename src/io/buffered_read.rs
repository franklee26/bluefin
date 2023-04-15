use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::Poll,
};

use rand::Rng;

use crate::core::packet::Packet;

use super::manager::ConnectionBuffer;

pub struct BufferedRead {
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
