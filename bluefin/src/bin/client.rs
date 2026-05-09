#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    env,
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

use bluefin::net::client::BluefinClient;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::Bytes;
use tokio::{spawn, task::yield_now, time::sleep};

/// Client-side source ports. One per spawned connection task.
const DEFAULT_PORTS: [u16; 5] = [1320, 1322, 1323, 1324, 1325];
const SERVER_PORT: u16 = 1318;
/// Yield to the Tokio runtime every N sends so we don't monopolize the worker
/// thread. `BluefinConnection::send` is synchronous (just enqueues), so without
/// an explicit yield the only scheduling point per task is the periodic sleep.
const SENDS_PER_YIELD: usize = 256;

/// CLI:
///   client                    -> spawn 2 tasks in this process (default, like before)
///   client --task <ix>        -> spawn ONE task using DEFAULT_PORTS[ix].
///                                Use this to run each connection in its own
///                                process and avoid intra-runtime starvation.
#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // console_subscriber::init();
    let args: Vec<String> = env::args().collect();
    let single_task_index: Option<usize> = match args.get(1).map(String::as_str) {
        Some("--task") => Some(
            args.get(2)
                .expect("--task expects an index")
                .parse()
                .expect("--task index must be a usize"),
        ),
        Some(other) => panic!("unknown arg: {}", other),
        None => None,
    };

    let mut connection_tasks = vec![];

    let task_indices: Vec<usize> = match single_task_index {
        Some(ix) => {
            assert!(ix < DEFAULT_PORTS.len(), "task index out of range");
            vec![ix]
        }
        None => (0..2).collect(),
    };

    for (spawn_ix, task_ix) in task_indices.into_iter().enumerate() {
        // Small delay to ensure server has both accept() calls ready before
        // the second connection's hello arrives.
        if spawn_ix > 0 {
            sleep(Duration::from_millis(100)).await;
        }
        let port = DEFAULT_PORTS[task_ix];
        let connection_task = spawn(async move {
            run_connection(task_ix, port).await
        });
        connection_tasks.push(connection_task);
    }

    let mut had_failure = false;
    for task in connection_tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("(client) connection task returned error: {:?}", e);
                had_failure = true;
            }
            Err(join_err) => {
                eprintln!("(client) connection task join error: {:?}", join_err);
                had_failure = true;
            }
        }
    }

    if had_failure {
        // Surface the failure to the shell so the bench script can detect it.
        std::process::exit(1);
    }

    Ok(())
}

async fn run_connection(task_ix: usize, src_port: u16) -> Result<(), BluefinError> {
    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        src_port,
    )));

    let mut conn = match client
        .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            SERVER_PORT,
        )))
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "(client #{}) connect() failed (src_port {}): {:?}",
                task_ix, src_port, e
            );
            return Err(e);
        }
    };

    let mut total_bytes = 0;

    // Tiny warm-up sends (kept from the original benchmark).
    total_bytes += conn.send(&[1, 2, 3, 4, 5, 6, 7])?;
    total_bytes += conn.send(&[12, 12, 12, 12, 12, 12])?;
    total_bytes += conn.send(&[13; 100])?;
    sleep(Duration::from_secs(1)).await;
    total_bytes += conn.send(&[14, 14, 14, 14, 14, 14])?;

    // Main payload loop.
    //
    // Build the payload once as a `Bytes` and `clone()` it per send. Cloning a
    // `Bytes` is a refcount bump, NOT a copy, so the only per-iteration cost
    // is the mpsc enqueue inside `send_bytes_async`. This is what unlocks the
    // win from the writer-channel `Vec<u8>` -> `Bytes` migration: the bench
    // can hand the writer an already-owned buffer instead of forcing it to
    // allocate + memcpy 1500 B every send.
    //
    // We use the async variant because the writer's send channel is now
    // bounded: when the queue fills, `send_bytes_async` awaits backpressure
    // instead of erroring or dropping. This caps memory growth on bursty
    // producers; without it, an unbounded channel could swallow gigabytes of
    // payloads that the writer hasn't shipped yet.
    let payload: Bytes = Bytes::from_static(&[0u8; 1500]);
    let start = Instant::now();
    const NUM_SENDS: usize = 10_000_000;
    for i in 0..NUM_SENDS {
        total_bytes += conn.send_bytes_async(payload.clone()).await?;
        // Yield often enough that other tasks (the other connection, the
        // writer pump, the reader, etc.) can actually run. Without this, a
        // tight `for` loop monopolises the worker thread.
        if i % SENDS_PER_YIELD == 0 {
            yield_now().await;
        }
    }

    // Wait for the writer pipeline to drain before exiting. The bounded send
    // channel only bounds the channel itself; the writer's internal
    // `data_queue` deque and bytes mid-`socket.send()` can still be in flight
    // when this loop returns. There's no public flush API yet, so a fixed
    // sleep is the simplest correct choice. (Without this, the process exits
    // mid-flight and the server reports a sharp delivered-bytes shortfall.)
    sleep(Duration::from_secs(2)).await;

    let elapsed = start.elapsed().as_secs_f64();
    let mb_per_sec = (total_bytes as f64 / elapsed) / 1e6;
    eprintln!(
        "(client #{}) sent {} bytes ({} sends) in {:.3} s ~ {:.2} mb/s",
        task_ix, total_bytes, NUM_SENDS, elapsed, mb_per_sec
    );

    Ok(())
}
