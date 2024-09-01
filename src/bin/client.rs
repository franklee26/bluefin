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
                Ipv4Addr::new(10, 0, 0, 31),
                ports[ix],
            )));
            let conn = client
                .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(10, 0, 0, 31),
                    1318,
                )))
                .await?;

            if ix == 0 {
                let bytes = [1, 2, 3, 4, 3, 2, 1];
                let mut size = conn.send(&bytes).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[1; 10]).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[2; 5]).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[3; 8]).await?;
                println!("Sent {} bytes", size);

                sleep(Duration::from_secs(2)).await;

                size = conn.send(&[12, 12, 12, 12, 12, 12]).await?;
                println!("Sent {} bytes", size);

                sleep(Duration::from_secs(3)).await;

                size = conn.send(&[14, 14, 14, 14, 14, 14]).await?;
                println!("Sent {} bytes", size);
            } else {
                let bytes = [7, 7, 7];
                let mut size = conn.send(&bytes).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[6; 10]).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[8; 5]).await?;
                println!("Sent {} bytes", size);

                size = conn.send(&[9; 8]).await?;
                println!("Sent {} bytes", size);

                sleep(Duration::from_secs(3)).await;

                size = conn.send(&[13, 13, 13, 13, 13, 13]).await?;
                println!("Sent {} bytes", size);
            }

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
