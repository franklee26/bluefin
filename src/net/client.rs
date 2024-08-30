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

pub struct BluefinClient {
    socket: Option<Arc<UdpSocket>>,
    src_addr: SocketAddr,
    dst_addr: Option<SocketAddr>,
    rx: Option<RxChannel>,
    conn_called: bool,
}

impl BluefinClient {
    pub fn new(src_addr: SocketAddr) -> Self {
        Self {
            socket: None,
            dst_addr: None,
            rx: None,
            conn_called: false,
            src_addr,
        }
    }

    pub async fn connect(&mut self, dst_addr: SocketAddr) -> BluefinResult<()> {
        let buffer = ConnectionBuffer::new();

        let socket = Arc::new(UdpSocket::bind(self.src_addr).await?);
        socket.connect(dst_addr).await?;
        self.socket = Some(Arc::clone(&socket));

        let (mut tx, rx) = ReaderChannel::new(
            Arc::clone(self.socket.as_ref().unwrap()),
            Arc::new(Mutex::new(buffer)),
        );

        self.rx = Some(rx);

        spawn(async move {
            let _ = tx.run().await;
        });

        self.dst_addr = Some(dst_addr);
        self.conn_called = true;
        Ok(())
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> BluefinResult<usize> {
        if !self.conn_called {
            return Err(BluefinError::InvalidSocketError);
        }

        let (bytes, _) = self.rx.as_ref().unwrap().read().await;

        let size = buf.as_mut().write(&bytes)?;

        return Ok(size);
    }

    pub async fn send(&self, buf: &[u8]) -> BluefinResult<usize> {
        if !self.conn_called {
            return Err(BluefinError::InvalidSocketError);
        }

        // create bluefine packet and send
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let header = BluefinHeader::new(0x0, 0x0, PacketType::Data, 0x0, security_fields);
        let packet = BluefinPacket::builder()
            .header(header)
            .payload(buf.to_vec())
            .build();
        let serialised = packet.serialise();
        eprintln!("Sending {:?}", serialised);
        let bytes = self.socket.as_ref().unwrap().send(&serialised).await?;

        Ok(bytes)
    }
}
