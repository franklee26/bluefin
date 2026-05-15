#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use std::{
    env,
    net::{Ipv4Addr, SocketAddrV4},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use bluefin::net::client::BluefinClient;
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::Bytes;
use tokio::{spawn, task::yield_now, time::sleep};

/// Set to `true` by the SIGINT handler installed in `main`. The send loop
/// in [`run_connection`] checks this every iteration so a Ctrl-C cleanly
/// triggers `flush()` + `close()` instead of orphaning the connection.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Client-side source ports. One per spawned connection task.
const DEFAULT_PORTS: [u16; 5] = [1320, 1322, 1323, 1324, 1325];
const SERVER_PORT: u16 = 1318;
/// Yield to the Tokio runtime every N sends so we don't monopolize the worker
/// thread. `BluefinConnection::send` is synchronous (just enqueues), so without
/// an explicit yield the only scheduling point per task is the periodic sleep.
const SENDS_PER_YIELD: usize = 256;

/// Default number of 1500-byte payloads each connection sends in the main
/// payload loop. 10 M = 15 GB of application data per conn, which is the
/// canonical bench target on dev hardware.
///
/// CI overrides this via `BLUEFIN_NUM_SENDS` because hosted macos-latest
/// runners can't sustain enough throughput to deliver 15 GB inside the
/// server's recv-idle window. CI ships ~500 K (= 750 MB) so conns finish
/// naturally instead of being TRUNCated by the timeout. Production traffic
/// is unaffected unless the env var is explicitly set.
const DEFAULT_NUM_SENDS: usize = 10_000_000;

fn num_sends() -> usize {
    env::var("BLUEFIN_NUM_SENDS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_NUM_SENDS)
}

/// Optional per-batch throttle, in **microseconds**, read from
/// `BLUEFIN_CLIENT_SEND_THROTTLE_US`. When set to a positive integer the
/// client send loop sleeps that many microseconds every
/// `send_throttle_every()` iterations (see below). Default `0` (no
/// throttle) so production / dev-box behaviour is unchanged.
///
/// **Why this exists.** Bluefin has no on-wire congestion control yet
/// (see bluefin-architecture §8 "Congestion control: None"). On a healthy
/// dev box this is fine — the writer's bounded `flume::bounded(4096)`
/// queue and the kernel UDP socket buffers absorb bursts. On contended
/// hosted-CI runners (3 vCPUs, see bluefin-ci), the reader and writer
/// tasks are co-scheduled with everything else; a tight client send loop
/// can overwhelm the receiver's drain rate, overflow the kernel UDP
/// buffer, and trigger packet loss that bluefin's retransmit-free
/// pipeline can't recover from.
///
/// **Tuning note.** `tokio::time::sleep` has a multi-millisecond
/// granularity floor in practice (timer-wheel + scheduling overhead),
/// so any positive throttle value engages a roughly equivalent
/// per-batch delay regardless of the requested µs. The *cadence* — how
/// many sends per sleep — therefore matters far more than the sleep
/// length itself. Empirical sweep on Apple-silicon dev (25 µs sleep,
/// ~1.44 GB/s unthrottled baseline): every 256 → ~250 MB/s; every 1024
/// → ~650 MB/s; every 2048 → ~1.0 GB/s; every 4096 → ~1.2 GB/s.
const DEFAULT_SEND_THROTTLE_US: u64 = 0;

/// Default cadence for the throttle (number of sends between sleeps).
/// Only used when [`send_throttle_us`] is > 0. The default matches
/// [`SENDS_PER_YIELD`] so a small throttle gives a tight rate cap; CI
/// typically raises this to relax the cap once the basic throttling has
/// addressed receiver-overrun.
const DEFAULT_SEND_THROTTLE_EVERY: usize = SENDS_PER_YIELD;

fn send_throttle_us() -> u64 {
    env::var("BLUEFIN_CLIENT_SEND_THROTTLE_US")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SEND_THROTTLE_US)
}

fn send_throttle_every() -> usize {
    env::var("BLUEFIN_CLIENT_SEND_THROTTLE_EVERY")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SEND_THROTTLE_EVERY)
}

/// CLI:
///   client                    -> spawn 2 tasks in this process (default, like before)
///   client --task <ix>        -> spawn ONE task using DEFAULT_PORTS[ix].
///                                Use this to run each connection in its own
///                                process and avoid intra-runtime starvation.
#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // SIGINT handler: flip the global shutdown flag and let the
    // connection tasks exit their send loop. They will still call
    // `flush()` + `close()` so the server observes a clean FIN.
    spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("(client) ^C received — finishing in-flight sends then closing");
            SHUTDOWN.store(true, Ordering::Release);
        }
    });

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

    for (_spawn_ix, task_ix) in task_indices.into_iter().enumerate() {
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
    let n_sends = num_sends();
    if n_sends != DEFAULT_NUM_SENDS {
        eprintln!(
            "(client #{}) NUM_SENDS overridden via env: {} (default {})",
            task_ix, n_sends, DEFAULT_NUM_SENDS,
        );
    }
    let throttle_us = send_throttle_us();
    let throttle_every = send_throttle_every();
    let throttle = if throttle_us > 0 {
        eprintln!(
            "(client #{}) SEND_THROTTLE_US set: {} us every {} sends",
            task_ix, throttle_us, throttle_every,
        );
        Some(Duration::from_micros(throttle_us))
    } else {
        None
    };
    for i in 0..n_sends {
        total_bytes += conn.send_bytes_async(payload.clone()).await?;
        // Yield often enough that other tasks (the other connection, the
        // writer pump, the reader, etc.) can actually run. Without this, a
        // tight `for` loop monopolises the worker thread.
        if i % SENDS_PER_YIELD == 0 {
            yield_now().await;
            // Cheap relaxed check is fine here; we only need to notice
            // shutdown within one yield window. Break out so the rest of
            // the function still runs (flush + close).
            if SHUTDOWN.load(Ordering::Acquire) {
                eprintln!(
                    "(client #{}) shutdown observed at send {}/{} — stopping early",
                    task_ix, i, n_sends,
                );
                break;
            }
        }
        // Optional pacing on its OWN cadence (independent of the yield
        // cadence). Sleep gives the peer + local writer a real
        // scheduling window beyond what `yield_now()` provides. The
        // separation matters because `tokio::time::sleep` has a ~1 ms
        // floor: a tight cadence (e.g. every 256) caps throughput
        // aggressively (~384 MB/s), while a looser one (e.g. every
        // 1024–2048) only kicks in on true bursts above ~1.5–3 GB/s.
        if let Some(d) = throttle {
            if i % throttle_every == 0 {
                sleep(d).await;
            }
        }
    }

    // Wait for the writer pipeline to drain before exiting. `flush().await`
    // returns exactly when every byte we handed to `send_bytes_async` is on
    // the wire (channel-queue + writer's internal `data_queue` + spawned
    // sender's mid-`socket.send()` bytes). Replaces the old fixed
    // `sleep(2 s)`, which was visibly too short on contended runs (server
    // received a fraction of the bytes the client claimed to send).
    conn.flush().await?;

    // Graceful close per bluefin-protocol §10bis. Sends a `Fin` (header-
    // only, no payload, packet number = next-after-data) and waits for
    // the peer's `FinAck`. This is what lets the server exit immediately
    // on this connection's `recv` instead of having to fall back to its
    // recv-idle timer. Errors here are non-fatal for the bench — log and
    // continue so we still report the throughput we measured.
    if let Err(e) = conn.close().await {
        eprintln!("(client #{}) close() returned {:?} -- continuing", task_ix, e);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let mb_per_sec = (total_bytes as f64 / elapsed) / 1e6;
    eprintln!(
        "(client #{}) sent {} bytes ({} sends) in {:.3} s ~ {:.2} mb/s",
        task_ix, total_bytes, n_sends, elapsed, mb_per_sec
    );

    Ok(())
}
