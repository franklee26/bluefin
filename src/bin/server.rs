#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use bluefin::{net::server::BluefinServer, utils::common::BluefinResult};
use std::time::Duration;
use std::{
    cmp::{max, min},
    net::{Ipv4Addr, SocketAddrV4},
    time::Instant,
};
use tokio::task::yield_now;
use tokio::time::sleep;
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
    server.set_num_reader_workers(3)?;
    server.bind().await?;
    let mut join_set = JoinSet::new();

    let mut _num = 0;
    while let Ok(mut conn) = server.accept().await {
        let _ = join_set.spawn(async move {
                    let mut total_bytes = 0;
                    let mut recv_bytes = [0u8; 10000];
                    let mut min_bytes = usize::MAX;
                    let mut max_bytes = 0;
                    let mut iteration: i64 = 1;
                    let mut num_iterations_without_print = 0;
                    let mut max_throughput = 0.0;
                    let mut min_throughput = f64::MAX;
                    let now = Instant::now();
                    loop {
                        let size = conn.recv(&mut recv_bytes, 10000).await.unwrap();
                        total_bytes += size;
                        min_bytes = min(size, min_bytes);
                        max_bytes = max(size, max_bytes);
                        // eprintln!("read {} bytes --- total bytes: {}", size, total_bytes);

                        /*
                        println!(
                            "({:x}_{:x}) >>> Received: {} bytes",
                            conn.src_conn_id,
                            conn.dst_conn_id,
                            total_bytes
                        );
                        */
                        num_iterations_without_print += 1;
                        if total_bytes >= 1000000 && num_iterations_without_print == 3500 {
                            let elapsed = now.elapsed().as_secs();
                            if elapsed == 0 {
                                eprintln!("(#{})Total bytes: {} (0s???)", _num, total_bytes);
                                num_iterations_without_print = 0;
                                continue;
                            }
                            let through_put = u64::try_from(total_bytes).unwrap() / elapsed;
                            let through_put_mb = through_put as f64 / 1e6;
                            let avg_recv_bytes: f64 = total_bytes as f64 / iteration as f64;

                            if through_put_mb > max_throughput {
                                max_throughput = through_put_mb;
                            }

                            if through_put_mb < min_throughput {
                                min_throughput = through_put_mb;
                            }

                            if through_put_mb < 1000.0 {
                                eprintln!(
                                    "{} {:.1} kb/s or {:.1} mb/s (read {:.1} kb/iteration, min: {:.1} kb, max: {:.1} kb) (max {:.1} mb/s, min {:.1} mb/s)",
                                    _num,
                                    through_put as f64 / 1e3,
                                    through_put_mb,
                                    avg_recv_bytes / 1e3,
                                    min_bytes as f64 / 1e3,
                                    max_bytes as f64 / 1e3,
                                    max_throughput,
                                    min_throughput
                                );
                            } else {
                                eprintln!(
                                    "{} {:.2} gb/s (read {:.1} kb/iter, min: {:.1} kb, max: {:.1} kb) (max {:.2} gb/s, min {:.1} kb/s)",
                                    _num,
                                    through_put_mb / 1e3,
                                    avg_recv_bytes / 1e3,
                                    min_bytes as f64 / 1e3,
                                    max_bytes as f64 / 1e3,
                                    max_throughput / 1e3,
                                    min_throughput
                                );
                            }
                            num_iterations_without_print = 0;
                            // break;
                        }
                        iteration += 1;
                    }
        });
        _num += 1;
        if _num >= 2 {
            break;
        }
    }

    join_set.join_all().await;
    Ok(())
}
