use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::{net::server::BluefinServer, utils::common::BluefinResult};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> BluefinResult<()> {
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 0, 31),
        1318,
    )));
    server.bind().await?;

    loop {
        let mut recv_bytes = [0u8; 1024];
        let size = server.recv(&mut recv_bytes).await?;

        println!(">>> Received: {:?}", &recv_bytes[..size]);
        sleep(Duration::from_secs(1)).await;
    }
}
