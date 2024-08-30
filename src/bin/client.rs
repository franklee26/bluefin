use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::{net::client::BluefinClient, utils::common::BluefinResult};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> BluefinResult<()> {
    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 0, 31),
        1320,
    )));
    client
        .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 31),
            1318,
        )))
        .await?;

    let bytes = [1, 2, 3, 4, 3, 2, 1];
    let mut size = client.send(&bytes).await?;
    println!("Sent {} bytes", size);

    // sleep(Duration::from_secs(1)).await;

    size = client.send(&[1; 10]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[2; 5]).await?;
    println!("Sent {} bytes", size);

    // sleep(Duration::from_secs(1)).await;
    size = client.send(&[3; 8]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[4; 10]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[5; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[6; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[7; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[8; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[9; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[10; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[11; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[12; 20]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[13; 100]).await?;
    println!("Sent {} bytes", size);

    size = client.send(&[9, 8, 7, 6, 5]).await?;
    println!("Sent {} bytes", size);

    Ok(())
}
