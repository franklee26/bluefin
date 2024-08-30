use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use tokio::{net::UdpSocket, time::sleep};

use crate::{
    core::{packet::BluefinPacket, Serialisable},
    net::connection::{ConnectionBuffer, MAX_BUFFER_CONSUME},
    utils::common::BluefinResult,
};

#[derive(Clone)]
pub(crate) struct TxChannel {
    pub(crate) id: u8,
    socket: Arc<UdpSocket>,
    buffer: Arc<Mutex<ConnectionBuffer>>,
    waker: Arc<Mutex<Option<Waker>>>,
}

#[derive(Clone)]
pub(crate) struct RxChannel {
    buffer: Arc<Mutex<ConnectionBuffer>>,
    waker: Arc<Mutex<Option<Waker>>>,
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
        drop(guard);

        let mut waker_guard = self.waker.lock().unwrap();
        *waker_guard = Some(cx.waker().clone());
        drop(waker_guard);

        Poll::Pending
    }
}

impl RxChannel {
    pub(crate) async fn read(&self) -> (Vec<u8>, SocketAddr) {
        self.clone().await
    }
}

pub(crate) struct ReaderChannel {}

impl ReaderChannel {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        buffer: Arc<Mutex<ConnectionBuffer>>,
    ) -> (TxChannel, RxChannel) {
        let waker = Arc::new(Mutex::new(None));
        let tx = TxChannel {
            id: 0,
            socket: Arc::clone(&socket),
            buffer: Arc::clone(&buffer),
            waker: Arc::clone(&waker),
        };
        let rx = RxChannel {
            buffer: Arc::clone(&buffer),
            waker: Arc::clone(&waker),
        };
        (tx, rx)
    }
}

impl TxChannel {
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

            // We still have remaining data! Wake again! Only once???
            /*
            if let Some(w) = saved_waker.take() {
                w.wake();
            }
            */

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
            {
                let mut guard = self.buffer.lock().unwrap();
                let buffer_res = guard.buffer_in_packet(&mut packet);

                // Could not buffer in packet... buffer is likely full. We will have to discard the
                // packet.
                if let Err(e) = buffer_res {
                    eprintln!("{:?}", e);
                    encountered_err = true;
                    continue;
                }

                if let Err(e) = guard.buffer_in_addr(addr) {
                    // encountered_err = true;
                    // continue;
                }
            }

            {
                let waker_guard = self.waker.lock().unwrap();
                // Wake future that buffered data is available
                if let Some(w) = waker_guard.as_ref() {
                    w.wake_by_ref();
                }
            }
        }
    }
}
