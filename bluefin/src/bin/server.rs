#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
use bluefin::net::server::BluefinServer;
use bluefin_proto::BluefinResult;
use bytes::Bytes;
use std::{
    cmp::{max, min},
    env,
    net::{Ipv4Addr, SocketAddrV4},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::{spawn, task::JoinSet, time::sleep};

/// Set to `true` by the SIGINT handler installed in `main`. Per-conn
/// tasks observe this in the recv `select!` and exit cleanly via the
/// usual FINAL-line path so a Ctrl-C produces the same diagnostics as a
/// healthy run.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// If no bytes arrive on a connection for this long, the per-connection
/// task assumes the peer is gone, prints a final summary, and exits.
///
/// As of the FIN / FIN-ACK exchange landing this is a *safety net* for
/// a crashed/SIGKILL'd client — a healthy client now calls
/// `BluefinConnection::close()` and our `recv_bytes` returns `Ok(0)`
/// (EOF), at which point the per-conn task breaks immediately. The
/// timeout still exists so the bench server cannot hang forever if a
/// peer dies mid-stream.
///
/// CI overrides this via `BLUEFIN_RECV_IDLE_TIMEOUT_SECS` because hosted
/// macos-latest runners deliver bytes at ~1–10 % of dev-box throughput,
/// so the 15 GB target needs minutes, not seconds, to complete. Without
/// the override, every CI conn TRUNCs at the 2 s mark and the bench gate
/// degenerates into a smoke test. Production traffic is unaffected
/// unless the env var is explicitly set.
const DEFAULT_RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

fn recv_idle_timeout() -> Duration {
    env::var("BLUEFIN_RECV_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RECV_IDLE_TIMEOUT)
}

/// How many `ReaderTxChannel` workers the bench server binds to its
/// listening UDP socket. Reader workers demux datagrams to the right
/// `ConnectionBuffer` and are on the hot path during handshake; once a
/// `BluefinConnection`'s per-connection socket takes over, additional
/// reader workers buy nothing for steady-state throughput.
///
/// Default 3 is great on dev boxes (8–10 perf cores) but oversubscribes
/// hosted macos-latest runners (3 vCPUs total, shared between the server
/// and client processes plus tokio's worker threads). CI overrides this
/// via `BLUEFIN_NUM_READER_WORKERS` to keep a tiny bit of demux
/// parallelism without piling more threads than the runner has cores.
/// Production builds are unaffected unless the env var is explicitly set.
const DEFAULT_NUM_READER_WORKERS: u16 = 3;

fn num_reader_workers() -> u16 {
    env::var("BLUEFIN_NUM_READER_WORKERS")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_NUM_READER_WORKERS)
}

/// How often to push the idle deadline forward. Resetting the `Sleep`'s
/// deadline on every recv (the natural `tokio::time::timeout` pattern) re-arms
/// the timer wheel at recv-rate (~270 K/s on the bench), which dominates
/// CPU. Resetting once per `IDLE_RESET_EVERY` recvs caps that at <100 Hz while
/// keeping idle detection sharp to `RECV_IDLE_TIMEOUT + ~15 ms` at the bench's
/// recv rate.
const IDLE_RESET_EVERY: i64 = 4096;

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> BluefinResult<()> {
    // SIGINT handler: flip the global shutdown flag. Per-conn tasks
    // observe this on the next recv-loop tick and break out cleanly so
    // we still produce FINAL lines.
    spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("(server) ^C received — closing connections gracefully ...");
            SHUTDOWN.store(true, Ordering::Release);
        }
    });

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
    let n_workers = num_reader_workers();
    if n_workers != DEFAULT_NUM_READER_WORKERS {
        eprintln!(
            "(server) reader workers overridden via env: {} (default {})",
            n_workers, DEFAULT_NUM_READER_WORKERS,
        );
    }
    server.set_num_reader_workers(n_workers)?;
    server.bind().await?;
    let mut join_set = JoinSet::new();

    // How many client connections to accept before we move on to processing.
    //
    // CLI:    `server [N]`              (positional, first arg)
    // Env:    `BLUEFIN_BENCH_NUM_CONNS=N`
    // Default: 2
    //
    // The bench wrapper passes `-n` through as the positional arg so a
    // single sweep can drive 1..=N clients without recompiling. The env
    // var is the fallback for ad-hoc invocations (`BLUEFIN_BENCH_NUM_CONNS=4
    // ./target/release/server`). CLI > env > default.
    let num_expected_connections: usize = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .or_else(|| env::var("BLUEFIN_BENCH_NUM_CONNS").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(2);
    assert!(
        num_expected_connections >= 1,
        "num_expected_connections must be >= 1 (got {num_expected_connections})"
    );
    eprintln!(
        "(server) accepting {} connection(s) before starting recv loops",
        num_expected_connections,
    );
    let recv_idle = recv_idle_timeout();
    if recv_idle != DEFAULT_RECV_IDLE_TIMEOUT {
        eprintln!(
            "(server) recv-idle timeout overridden via env: {:?} (default {:?})",
            recv_idle, DEFAULT_RECV_IDLE_TIMEOUT,
        );
    }
    let mut connections = Vec::with_capacity(num_expected_connections);

    // Accept all connections FIRST before spawning any processing tasks
    // This avoids the race where processing one connection blocks accepting the next
    for _conn_num in 0..num_expected_connections {
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
            let recv_idle = recv_idle;
            let mut total_bytes: usize = 0;
            // Carrier vec for the zero-copy `recv_bytes` API. We keep
            // capacity across iterations by `drain(..)`-ing instead of
            // reassigning, so this allocates exactly once per connection.
            // 16 matches the per-call max-packets argument below.
            let mut chunks: Vec<Bytes> = Vec::with_capacity(16);
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

            // Single long-lived idle deadline. Pinning one `Sleep` and
            // resetting its deadline (instead of allocating a fresh `Sleep`
            // per recv via `tokio::time::timeout`) is the documented tokio
            // idiom for hot loops and removes the per-recv timer-wheel
            // arm/disarm.
            let idle_sleep = sleep(recv_idle);
            tokio::pin!(idle_sleep);

            // Tracks why we exited the recv loop. `true` means the peer
            // sent a `Fin` and `recv_bytes` returned `Ok(0)` — the
            // clean-shutdown path, which means the FINAL elapsed time
            // should NOT have the idle-timeout window subtracted from
            // it (there is no idle window in this path). `false` means
            // the idle deadline fired, in which case the historical
            // tail-subtraction still applies. Defaults to `false` so
            // that if the loop breaks via the `recv error` arm we keep
            // the conservative subtraction.
            let mut clean_eof = false;

            loop {
                // Cheap pre-check so a Ctrl-C between recv windows still
                // breaks out promptly. The select arms below also race
                // a short-lived sleep that lets the SIGINT-driven flag
                // wake the loop without waiting for the next recv.
                if SHUTDOWN.load(Ordering::Acquire) {
                    eprintln!("(#{}) shutdown observed — closing", _num);
                    break;
                }
                // `recv_bytes` is the zero-copy variant of `recv`: it pushes
                // `Bytes` slices over the recv buffer into our carrier vec
                // instead of memcpying into a `[u8]`. The bench has no need
                // for a contiguous buffer (we just sum lengths).
                let size = tokio::select! {
                    biased;
                    recv = conn.recv_bytes(&mut chunks, 16) => {
                        match recv {
                            Ok(size) => size,
                            Err(e) => {
                                eprintln!("(#{}) recv error: {:?} -- exiting", _num, e);
                                break;
                            }
                        }
                    }
                    _ = idle_sleep.as_mut() => {
                        eprintln!(
                            "(#{}) idle for {:?} -- assuming peer is gone",
                            _num, recv_idle
                        );
                        break;
                    }
                };
                // `recv_bytes` returning `Ok(0)` with an empty carrier vec
                // means the peer sent a `Fin` and there is nothing left to
                // drain (see [`BluefinConnection::recv_bytes`] EOF
                // semantics). Break out cleanly so the FINAL line reports
                // un-padded elapsed time.
                if size == 0 && chunks.is_empty() {
                    eprintln!("(#{}) peer closed (EOF) -- exiting", _num);
                    clean_eof = true;
                    break;
                }
                // Push the idle deadline forward on a coarse cadence so we
                // re-arm the timer wheel at <100Hz instead of recv-rate.
                if iteration % IDLE_RESET_EVERY == 0 {
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + recv_idle);
                }
                total_bytes += size;
                // Drain the carrier vec, tracking the smallest/largest
                // payload we've seen so far. `drain(..)` keeps the vec's
                // allocated capacity, so the next recv reuses it.
                for b in chunks.drain(..) {
                    let n = b.len();
                    min_bytes = min(n, min_bytes);
                    max_bytes = max(n, max_bytes);
                    // Refcount drop on `b` happens here; it's the only
                    // bookkeeping per-payload — no copy.
                }

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
            //
            // Two exit paths feed this block:
            //   1. Clean EOF: the client called `close()`, we observed a
            //      `Fin`, and `recv_bytes` returned `Ok(0)`. There is no
            //      idle tail in the elapsed time — use it as-is.
            //   2. Idle deadline: the client never sent a `Fin` (or
            //      crashed). `elapsed_raw` includes ~`recv_idle` seconds
            //      of waiting-for-nothing at the end; subtract it so the
            //      reported avg reflects real transfer time.
            //
            // Clamp to a minimum positive value so a transfer that was
            // fully consumed by the idle wait (i.e. zero real progress)
            // doesn't divide by zero or report a wildly inflated number.
            let elapsed_raw = now.elapsed().as_secs_f64();
            let elapsed = if clean_eof {
                elapsed_raw.max(1e-3)
            } else {
                (elapsed_raw - recv_idle.as_secs_f64()).max(1e-3)
            };
            let avg_throughput_mb = if total_bytes > 0 {
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

            // Mirror the client's graceful close so the peer observes a
            // `Fin` from our side too — but ONLY when we exited the recv
            // loop for a reason other than the peer already closing. If
            // `clean_eof` is true the client has already torn its side
            // down, deregistered itself, and is no longer listening for
            // our FIN; sending one would just waste 600 ms re-transmitting
            // before hitting the retransmit budget. In the
            // shutdown / idle / error paths the peer may still be alive,
            // so closing produces a real FIN that the client's recv can
            // observe.
            if !clean_eof {
                if let Err(e) = conn.close().await {
                    eprintln!("(#{}) close() returned {:?}", _num, e);
                }
            }
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
