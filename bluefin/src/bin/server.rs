#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use bluefin::net::server::BluefinServer;
use bluefin_proto::BluefinResult;
use std::{
    cmp::{max, min},
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};
use tokio::{spawn, task::JoinSet, time::timeout};

/// If no bytes arrive on a connection for this long, the per-connection
/// task assumes the peer is gone, prints a final summary, and exits.
/// Bluefin currently has no protocol-level FIN, so this is the only way
/// the benchmark server can terminate cleanly.
const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

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

    const NUM_EXPECTED_CONNECTIONS: usize = 2;
    let mut connections = Vec::with_capacity(NUM_EXPECTED_CONNECTIONS);
    
    // Accept all connections FIRST before spawning any processing tasks
    // This avoids the race where processing one connection blocks accepting the next
    for _conn_num in 0..NUM_EXPECTED_CONNECTIONS {
        match server.accept().await {
            Ok(conn) => {
                connections.push((_conn_num, conn));
            }
            Err(_e) => {
                // Connection failed to accept
            }
        }
    }
    
    // Now spawn processing tasks for all accepted connections
    for (conn_num, mut conn) in connections {
        let _ = join_set.spawn(async move {
            let _num = conn_num;
            let mut total_bytes: usize = 0;
            let mut recv_bytes = [0u8; 10000];
            let mut min_bytes = usize::MAX;
            let mut max_bytes = 0;
            let mut iteration: i64 = 1;
            let mut num_iterations_without_print = 0;
            let mut max_throughput: f64 = 0.0;
            let mut min_throughput: f64 = f64::MAX;
            let now = Instant::now();
            // Track bytes/time at the previous print so we can report
            // *instantaneous* throughput (since last print) alongside the
            // cumulative running average.
            let mut last_print_bytes: usize = 0;
            let mut last_print_instant = now;
            loop {
                // Use a timeout so the server self-terminates when the client
                // stops sending. UDP gives us no close signal and Bluefin has
                // no protocol-level FIN yet.
                let recv_result =
                    timeout(RECV_IDLE_TIMEOUT, conn.recv(&mut recv_bytes, 10000)).await;
                let size = match recv_result {
                    Ok(Ok(size)) => size,
                    Ok(Err(e)) => {
                        eprintln!("(#{}) recv error: {:?} -- exiting", _num, e);
                        break;
                    }
                    Err(_) => {
                        eprintln!(
                            "(#{}) idle for {:?} -- assuming peer is gone",
                            _num, RECV_IDLE_TIMEOUT
                        );
                        break;
                    }
                };
                total_bytes += size;
                min_bytes = min(size, min_bytes);
                max_bytes = max(size, max_bytes);

                num_iterations_without_print += 1;
                // Use >= so we always emit a line at or after the threshold,
                // even if the loop exits before hitting the exact value again.
                if total_bytes >= 1_000_000 && num_iterations_without_print >= 3500 {
                    let now_inst = Instant::now();
                    print_throughput(
                        _num,
                        total_bytes,
                        iteration,
                        min_bytes,
                        max_bytes,
                        now.elapsed().as_secs_f64(),
                        total_bytes - last_print_bytes,
                        (now_inst - last_print_instant).as_secs_f64(),
                        &mut max_throughput,
                        &mut min_throughput,
                    );
                    last_print_bytes = total_bytes;
                    last_print_instant = now_inst;
                    num_iterations_without_print = 0;
                }
                iteration += 1;
            }

            // Final summary on exit so we always see a meaningful number,
            // even for short-lived connections.
            let elapsed = now.elapsed().as_secs_f64();
            let avg_throughput_mb = if elapsed > 0.0 {
                (total_bytes as f64 / elapsed) / 1e6
            } else {
                0.0
            };
            eprintln!(
                "(#{}) FINAL: {} bytes in {:.3} s -- avg {} (peak {})",
                _num,
                total_bytes,
                elapsed,
                fmt_throughput_mb(avg_throughput_mb),
                fmt_throughput_mb(max_throughput),
            );
        });
    }

    join_set.join_all().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_throughput(
    conn_num: usize,
    total_bytes: usize,
    iteration: i64,
    min_bytes: usize,
    max_bytes: usize,
    elapsed_secs: f64,
    window_bytes: usize,
    window_secs: f64,
    max_throughput: &mut f64,
    min_throughput: &mut f64,
) {
    // Avoid divide-by-zero in the sub-millisecond window.
    if elapsed_secs <= 0.0 {
        eprintln!(
            "(#{}) Total bytes: {} (elapsed {:.6} s, throughput indeterminate)",
            conn_num, total_bytes, elapsed_secs
        );
        return;
    }
    let avg_bytes_per_sec = total_bytes as f64 / elapsed_secs;
    let avg_mb = avg_bytes_per_sec / 1e6;
    let avg_recv_bytes = total_bytes as f64 / iteration as f64;
    // Instantaneous throughput over the window since the previous print.
    let inst_mb = if window_secs > 0.0 {
        (window_bytes as f64 / window_secs) / 1e6
    } else {
        0.0
    };

    // Track peak/trough using the *instantaneous* number — the cumulative
    // average is monotone-ish and not very informative as a max/min.
    if inst_mb > *max_throughput {
        *max_throughput = inst_mb;
    }
    if inst_mb < *min_throughput {
        *min_throughput = inst_mb;
    }

    eprintln!(
        "{} avg {} | inst {} (read {:.1} kb/iter, min: {:.1} kb, max: {:.1} kb) (peak {}, trough {})",
        conn_num,
        fmt_throughput_mb(avg_mb),
        fmt_throughput_mb(inst_mb),
        avg_recv_bytes / 1e3,
        min_bytes as f64 / 1e3,
        max_bytes as f64 / 1e3,
        fmt_throughput_mb(*max_throughput),
        fmt_throughput_mb(*min_throughput),
    );
}

/// Format a throughput value (already in MB/s) at a sensible scale.
fn fmt_throughput_mb(mb_per_sec: f64) -> String {
    if mb_per_sec >= 1000.0 {
        format!("{:.2} gb/s", mb_per_sec / 1e3)
    } else if mb_per_sec >= 1.0 {
        format!("{:.1} mb/s", mb_per_sec)
    } else {
        format!("{:.1} kb/s", mb_per_sec * 1e3)
    }
}
