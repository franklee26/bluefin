#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::net::client::BluefinClient;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use tokio::{spawn, time::sleep};

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // console_subscriber::init();
    let ports = [1320, 1322, 1323, 1324, 1325];
    let mut connection_tasks = vec![];
    
    // Start connections with a small delay to avoid racing the server's accept() calls
    for ix in 0..2 {
        // Small delay to ensure server has both accept() calls ready
        if ix > 0 {
            sleep(Duration::from_millis(100)).await;
        }
        let port = ports[ix];
        let connection_task = spawn(async move {
            let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                port,
            )));
            
            match client
                .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(127, 0, 0, 1),
                    1318,
                )))
                .await
            {
                Ok(mut conn) => {
                    let mut total_bytes = 0;

                    let bytes = [1, 2, 3, 4, 5, 6, 7];
                    let mut size = conn.send(&bytes)?;
                    total_bytes += size;

                    size = conn.send(&[12, 12, 12, 12, 12, 12])?;
                    total_bytes += size;

                    size = conn.send(&[13; 100])?;
                    total_bytes += size;

                    sleep(Duration::from_secs(1)).await;

                    size = conn.send(&[14, 14, 14, 14, 14, 14])?;
                    total_bytes += size;

                    let my_array = [0u8; 1500];
                    for i in 0..10000000 {
                        // let my_array: [u8; 32] = rand::random();
                        size = conn.send(&my_array)?;
                        total_bytes += size;
                        if i % 4000 == 0 {
                            sleep(Duration::from_millis(1)).await;
                        }
                    }
                    sleep(Duration::from_secs(3)).await;

                    Ok::<(), BluefinError>(())
                }
                Err(e) => {
                    Err(e)
                }
            }
        });
        connection_tasks.push(connection_task);
    }

    // Wait for all connection attempts to complete
    for (_ix, task) in connection_tasks.into_iter().enumerate() {
        let _ = task.await;
    }

    Ok(())
}
