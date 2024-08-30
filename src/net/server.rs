use std::{
    io::Write,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{net::UdpSocket, spawn};

use crate::{
    core::{
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        Serialisable,
    },
    utils::common::BluefinResult,
    worker::reader::{ReaderChannel, RxChannel},
};

use super::connection::ConnectionBuffer;

pub struct BluefinServer {
    socket: Option<Arc<UdpSocket>>,
    bind_called: bool,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    rx: Option<RxChannel>,
    client_hello_handled: bool,
}

impl BluefinServer {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            rx: None,
            dst_addr: None,
            bind_called: false,
            client_hello_handled: false,
            src_addr,
        }
    }

    pub async fn bind(&mut self) -> BluefinResult<()> {
        let buffer = ConnectionBuffer::new();

        let socket = UdpSocket::bind(self.src_addr).await?;
        self.socket = Some(Arc::new(socket));

        let (tx, rx) = ReaderChannel::new(
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::new(Mutex::new(buffer)),
        );

        self.rx = Some(rx);

        for i in 0..10 {
            let mut tx_clone = tx.clone();
            tx_clone.id = i;
            spawn(async move {
                let _ = tx_clone.run().await;
            });
        }

        self.bind_called = true;
        Ok(())
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> BluefinResult<usize> {
        if !self.bind_called {
            return Err(BluefinError::InvalidSocketError);
        }

        if self.rx.is_none() {
            return Err(BluefinError::BufferDoesNotExist);
        }

        let (bytes, addr) = self.rx.as_ref().unwrap().read().await;

        // Need to set the destination address
        if !self.client_hello_handled {
            self.dst_addr = Some(addr);
            self.socket
                .as_ref()
                .unwrap()
                .connect(self.dst_addr.unwrap())
                .await?;
            self.client_hello_handled = true;
        }

        let size = buf.as_mut().write(&bytes)?;

        return Ok(size);
    }

    pub async fn send(&self, buf: &[u8]) -> BluefinResult<usize> {
        if !self.bind_called {
            return Err(BluefinError::InvalidSocketError);
        }

        if !self.client_hello_handled {
            return Err(BluefinError::WriteError(
                "Cannot send bytes across socket as client hello has not completed".to_string(),
            ));
        }

        // create bluefine packet and send
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let header = BluefinHeader::new(0x0, 0x0, PacketType::Data, 0x0, security_fields);
        let packet = BluefinPacket::builder()
            .header(header)
            .payload(buf.to_vec())
            .build();
        let bytes = self
            .socket
            .as_ref()
            .unwrap()
            .send(&packet.serialise())
            .await?;

        Ok(bytes)
    }
}
