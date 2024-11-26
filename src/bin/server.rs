#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    cmp::{max, min},
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

use bluefin::{net::server::BluefinServer, utils::common::BluefinResult};
use tokio::{spawn, time::sleep};

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // console_subscriber::init();
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        1318,
    )));
    server.set_num_reader_workers(50)?;
    server.bind().await?;

    const MAX_NUM_CONNECTIONS: usize = 3;
    for conn_num in 0..MAX_NUM_CONNECTIONS {
        let mut s = server.clone();
        let _ = spawn(async move {
            let _num = conn_num;
            loop {
                let _conn = s.accept().await;

                match _conn {
                    Ok(mut conn) => {
                        spawn(async move {
                            let mut total_bytes = 0;
                            let mut recv_bytes = [0u8; 500000];
                            let mut min_bytes = usize::MAX;
                            let mut max_bytes = 0;
                            let mut iteration = 1;
                            let now = Instant::now();
                            loop {
                                // eprintln!("Waiting...");
                                let size = conn.recv(&mut recv_bytes, 500000).await.unwrap();
                                total_bytes += size;
                                min_bytes = min(size, min_bytes);
                                max_bytes = max(size, max_bytes);
                                // eprintln!("read {} bytes --- total bytes: {}", size, total_bytes);

                                /*
                                println!(
                                    "({:x}_{:x}) >>> Received: {:?} (total: {})",
                                    conn.src_conn_id,
                                    conn.dst_conn_id,
                                    &recv_bytes[..size],
                                    total_bytes
                                );
                                */
                                if total_bytes >= 100000 {
                                    let elapsed = now.elapsed().as_secs();
                                    let through_put = u64::try_from(total_bytes).unwrap() / elapsed;
                                    let avg_recv_bytes: f64 = total_bytes as f64 / iteration as f64;
                                    eprintln!(
                                        "{} {:.1} kb/s or {:.1} mb/s (read {:.1} kb/iteration, min: {:.1} kb, max: {:.1} kb)",
                                        _num,
                                        through_put as f64 / 1e3,
                                        through_put as f64 / 1e6,
                                        avg_recv_bytes / 1e3,
                                        min_bytes as f64 / 1e3,
                                        max_bytes as f64 / 1e3
                                    );
                                    // break;
                                }
                                iteration += 1;
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Could not accept connection due to error: {:?}", e);
                    }
                }

                sleep(Duration::from_secs(1)).await;
            }
        });
    }

    // The spawned tasks are looping forever. This infinite loop will keep the
    // process up forever.
    loop {}
}
