#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    cmp::{max, min},
    net::{Ipv4Addr, SocketAddrV4},
    time::Instant,
};

use bluefin::{net::server::BluefinServer, utils::common::BluefinResult};
use tokio::{spawn, task::JoinSet};

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    let _ = spawn(async move {
        let _ = run().await;
    })
    .await;
    Ok(())
}

async fn run() -> BluefinResult<()> {
    // console_subscriber::init();
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        1318,
    )));
    server.set_num_reader_workers(5)?;
    server.bind().await?;
    let mut join_set = JoinSet::new();

    const MAX_NUM_CONNECTIONS: usize = 2;
    for conn_num in 0..MAX_NUM_CONNECTIONS {
        let mut s = server.clone();
        let _num = conn_num;
        let _ = join_set.spawn(async move {
            let _conn = s.accept().await;

            match _conn {
                Ok(mut conn) => {
                    let mut total_bytes = 0;
                    let mut recv_bytes = [0u8; 80000];
                    let mut min_bytes = usize::MAX;
                    let mut max_bytes = 0;
                    let mut iteration = 1;
                    let mut num_iterations_without_print = 0;
                    let now = Instant::now();
                    loop {
                        let size = conn.recv(&mut recv_bytes, 80000).await.unwrap();
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
                        num_iterations_without_print += 1;
                        if total_bytes >= 100000 && num_iterations_without_print == 500 {
                            let elapsed = now.elapsed().as_secs();
                            let through_put = u64::try_from(total_bytes).unwrap() / elapsed;
                            let through_put_mb = through_put as f64 / 1e6;
                            let avg_recv_bytes: f64 = total_bytes as f64 / iteration as f64;
                            if through_put_mb < 1000.0 {
                            eprintln!(
                                    "{} {:.1} kb/s or {:.1} mb/s (read {:.1} kb/iteration, min: {:.1} kb, max: {:.1} kb)",
                                    _num,
                                    through_put as f64 / 1e3,
                                    through_put_mb,
                                    avg_recv_bytes / 1e3,
                                    min_bytes as f64 / 1e3,
                                    max_bytes as f64 / 1e3
                                );
                            } else {
                            eprintln!(
                                    "{} {:.1} gb/s (read {:.1} kb/iteration, min: {:.1} kb, max: {:.1} kb)",
                                    _num,
                                    through_put_mb / 1e3,
                                    avg_recv_bytes / 1e3,
                                    min_bytes as f64 / 1e3,
                                    max_bytes as f64 / 1e3
                                );
                            }
                                num_iterations_without_print = 0;
                            // break;
                        }
                        iteration += 1;
                    }
                }
                Err(e) => {
                    eprintln!("Could not accept connection due to error: {:?}", e);
                }
            }
        });
    }
    join_set.join_all().await;
    Ok(())
}
