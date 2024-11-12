use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::{
    core::error::BluefinError, net::client::BluefinClient, utils::common::BluefinResult,
};
use tokio::{spawn, time::sleep};

#[tokio::main]
async fn main() -> BluefinResult<()> {
    let ports = [1320, 1322];
    let mut tasks = vec![];
    for ix in 0..2 {
        // sleep(Duration::from_secs(3)).await;
        let task = spawn(async move {
            let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 37),
                ports[ix],
            )));
            let mut conn = client
                .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(127, 0, 0, 38),
                    1318,
                )))
                .await?;

            let bytes = [1, 2, 3, 4, 5, 6, 7];
            let mut size = conn.send(&bytes).await?;
            println!("Sent {} bytes", size);

            size = conn.send(&[12, 12, 12, 12, 12, 12]).await?;
            println!("Sent {} bytes", size);

            size = conn.send(&[13; 100]).await?;
            println!("Sent {} bytes", size);

            sleep(Duration::from_secs(5)).await;

            size = conn.send(&[14, 14, 14, 14, 14, 14]).await?;
            println!("Sent {} bytes", size);

            Ok::<(), BluefinError>(())
        });
        tasks.push(task);
    }

    for t in tasks {
        match t.await {
            Ok(r) => match r {
                Ok(()) => println!("Spawned task completed successfully"),
                Err(e) => eprintln!("Spawned task failed: {:?}", e),
            },
            Err(e) => eprintln!("Join handle failed: {:?}", e),
        }
    }

    Ok(())
}
