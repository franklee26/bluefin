#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::{
    core::error::BluefinError, net::client::BluefinClient, utils::common::BluefinResult,
};
use tokio::{spawn, time::sleep};

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // console_subscriber::init();
    let ports = [1320, 1322, 1323, 1324, 1325];
    let mut tasks = vec![];
    for ix in 0..2 {
        let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            ports[ix],
        )));
        if let Ok(mut conn) = client
            .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                1318,
            )))
            .await
        {
            let task = spawn(async move {
                let mut total_bytes = 0;

                let bytes = [1, 2, 3, 4, 5, 6, 7];
                let mut size = conn.send(&bytes)?;
                total_bytes += size;
                println!("Sent {} bytes", size);

                size = conn.send(&[12, 12, 12, 12, 12, 12])?;
                total_bytes += size;
                println!("Sent {} bytes", size);

                size = conn.send(&[13; 100])?;
                total_bytes += size;
                println!("Sent {} bytes", size);

                sleep(Duration::from_secs(1)).await;

                size = conn.send(&[14, 14, 14, 14, 14, 14])?;
                total_bytes += size;
                println!("Sent {} bytes", size);

                let my_array = [0u8; 1500];
                for ix in 0..10000000 {
                    // let my_array: [u8; 32] = rand::random();
                    size = conn.send(&my_array)?;
                    total_bytes += size;
                    if ix % 4000 == 0 {
                        sleep(Duration::from_millis(1)).await;
                    }
                }
                println!("Sent {} bytes", total_bytes);
                sleep(Duration::from_secs(3)).await;

                Ok::<(), BluefinError>(())
            });
            tasks.push(task);
            sleep(Duration::from_millis(1)).await;
        }
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
