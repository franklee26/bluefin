use std::{
    env,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use bluefin::net::client::BluefinClient;
use bluefin::net::server::BluefinServer;
use bluefin::net::DiagnosticEvent;
use bluefin_proto::BluefinResult;
use rand::Rng;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

/// When true, received data is printed as a hex dump alongside the UTF-8
/// representation, and send/recv events are annotated with byte counts.
static DIAGNOSTICS: AtomicBool = AtomicBool::new(false);

fn diag() -> bool {
    DIAGNOSTICS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

enum Mode {
    Listen { bind: String, port: u16 },
    Connect { host: String, port: u16, source_port: u16 },
}

fn print_usage() {
    eprintln!("bluefintool — netcat-like tool for the Bluefin protocol\n");
    eprintln!("USAGE:");
    eprintln!("  bluefintool -l <port>                     Listen on 127.0.0.1:<port>");
    eprintln!("  bluefintool -l <port> -b <bind_addr>      Listen on <bind_addr>:<port>");
    eprintln!("  bluefintool <host> <port>                 Connect to <host>:<port>");
    eprintln!("  bluefintool <host> <port> -s <src_port>   Connect from specific source port\n");
    eprintln!("OPTIONS:");
    eprintln!("  -l, --listen               Listen mode (accept multiple connections)");
    eprintln!("  -b, --bind <addr>          Bind address for listen mode [default: 127.0.0.1]");
    eprintln!("  -s, --source-port <port>   Source port for connect mode [default: random]");
    eprintln!("  -d, --diagnostics          Show hex dumps, byte counts, and peer tags for all traffic");
    eprintln!("  -h, --help                 Show this help");
}

fn parse_args() -> Mode {
    let mut args = env::args().skip(1);
    let mut listen = false;
    let mut bind = "127.0.0.1".to_string();
    let mut source_port: Option<u16> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-l" | "--listen" => listen = true,
            "-d" | "--diagnostics" => {
                DIAGNOSTICS.store(true, Ordering::Relaxed);
            }
            "-b" | "--bind" => {
                bind = args.next().unwrap_or_else(|| {
                    eprintln!("error: -b requires an address");
                    std::process::exit(1);
                });
            }
            "-s" | "--source-port" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("error: -s requires a port number");
                    std::process::exit(1);
                });
                source_port = Some(val.parse().unwrap_or_else(|_| {
                    eprintln!("error: invalid source port: {val}");
                    std::process::exit(1);
                }));
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            _ => positional.push(arg),
        }
    }

    if listen {
        let port: u16 = positional
            .first()
            .unwrap_or_else(|| {
                eprintln!("error: listen mode requires a port number");
                print_usage();
                std::process::exit(1);
            })
            .parse()
            .unwrap_or_else(|_| {
                eprintln!("error: invalid port number");
                std::process::exit(1);
            });
        Mode::Listen { bind, port }
    } else {
        if positional.len() != 2 {
            if positional.is_empty() {
                print_usage();
                std::process::exit(0);
            }
            eprintln!("error: connect mode requires <host> <port>");
            print_usage();
            std::process::exit(1);
        }
        let host = positional[0].clone();
        let port: u16 = positional[1].parse().unwrap_or_else(|_| {
            eprintln!("error: invalid port number: {}", positional[1]);
            std::process::exit(1);
        });
        let src = source_port.unwrap_or_else(|| {
            rand::rng().random_range(10000..60000u16)
        });
        Mode::Connect { host, port, source_port: src }
    }
}

// ---------------------------------------------------------------------------
// Bidirectional pipe: stdin → conn.send, conn.recv → stdout
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Hex dump helper
// ---------------------------------------------------------------------------

/// Prints a compact hex dump to stderr (16 bytes per line, with ASCII sidebar).
fn hex_dump(prefix: &str, data: &[u8]) {
    for (i, chunk) in data.chunks(16).enumerate() {
        let hex: String = chunk
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ascii: String = chunk
            .iter()
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
            .collect();
        eprintln!("{prefix} {:04x}  {hex:<48}  |{ascii}|", i * 16);
    }
}

/// Prints connection protocol details to stderr (diagnostic mode only).
fn dump_connection_info(
    conn: &bluefin::net::connection::BluefinConnection,
    role: &str,
    local_addr: &str,
    remote_addr: Option<&str>,
) {
    eprintln!("  local_addr  = {local_addr}");
    if let Some(remote) = remote_addr {
        eprintln!("  remote_addr = {remote}");
    }
    eprintln!("  src_conn_id = 0x{:08x}", conn.src_conn_id);
    eprintln!("  dst_conn_id = 0x{:08x}", conn.dst_conn_id);
    eprintln!("  role        = {role}");
    let sec = conn.security_fields();
    eprintln!("  version     = 0x{:x}", conn.version());
    eprintln!("  encrypted   = {}", sec.encrypted());
    eprintln!("  mask        = 0x{:02x}", sec.mask());
}

// ---------------------------------------------------------------------------
// Bidirectional pipe: stdin → conn.send, conn.recv → stdout
// ---------------------------------------------------------------------------

/// Runs a bidirectional pipe between stdin/stdout and the Bluefin connection.
///
/// If `stdin_rx` is `Some`, lines from stdin are sent to the peer. If `None`,
/// this connection is receive-only (used for the 2nd+ connections in listen
/// mode, since there's only one stdin).
///
/// `close_on_stdin_eof` controls what happens when a non-tty stdin reaches
/// EOF (e.g. `echo foo | bluefintool ...`). Connect mode passes `true` so
/// the one-shot pipeline terminates after the input is drained — matches
/// `nc -N` semantics. Listen mode passes `false`: the listener should not
/// die just because its (often-`Stdio::null`) stdin closed, so it stays
/// half-closed on the send side and keeps recv'ing until the peer FINs or
/// the user hits Ctrl-C.
///
/// Exits gracefully (by calling [`BluefinConnection::close`]) on any of:
/// * Ctrl-C / SIGINT (propagated via `shutdown`).
/// * Stdin EOF when `close_on_stdin_eof` is true (one-shot mode).
/// * Peer FIN, i.e. `conn.recv` returns `Ok(0)`.
/// * Unrecoverable send / recv error.
async fn run_pipe(
    mut conn: bluefin::net::connection::BluefinConnection,
    stdin_rx: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    mut shutdown: watch::Receiver<bool>,
    close_on_stdin_eof: bool,
) -> BluefinResult<()> {
    // Short tag that identifies the peer, e.g. "a1b2c3d4".
    let peer_tag = format!("{:08x}", conn.dst_conn_id);

    // Detect whether stdin is a terminal (interactive) or a pipe.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());

    let mut rx = stdin_rx;

    let mut buf = vec![0u8; 4096];
    let mut stdout = tokio::io::stdout();
    let mut total_sent: usize = 0;
    let mut total_recv: usize = 0;

    // Helper: drain all pending diagnostic events and print them.
    // Called right after send/recv so packet numbers appear next to
    // their associated data.
    let drain_diag = |conn: &bluefin::net::connection::BluefinConnection, peer_tag: &str| {
        if !diag() {
            return;
        }
        if let Some(diag_rx) = conn.diag_rx() {
            while let Ok(event) = diag_rx.try_recv() {
                match event {
                    DiagnosticEvent::AckReceived { base_packet_num, count } => {
                        eprintln!(
                            "[{peer_tag}] ack-recv: peer acked pkts 0x{base_packet_num:x}..+{count}"
                        );
                    }
                    DiagnosticEvent::AckSent { base_packet_num, count } => {
                        eprintln!(
                            "[{peer_tag}] ack-sent: we acked pkts 0x{base_packet_num:x}..+{count}"
                        );
                    }
                    DiagnosticEvent::DataSent { start_packet_num, num_packets, num_bytes } => {
                        eprintln!(
                            "[{peer_tag}] data-sent: pkts 0x{start_packet_num:x}..+{num_packets} ({num_bytes} bytes)"
                        );
                    }
                    DiagnosticEvent::DataReceived { base_packet_num, num_packets, num_bytes } => {
                        eprintln!(
                            "[{peer_tag}] data-recv: pkts 0x{base_packet_num:x}..+{num_packets} ({num_bytes} bytes)"
                        );
                    }
                    DiagnosticEvent::FinSent { packet_num } => {
                        eprintln!("[{peer_tag}] fin-sent: pkt 0x{packet_num:x}");
                    }
                    DiagnosticEvent::FinAckSent { packet_num } => {
                        eprintln!("[{peer_tag}] fin-ack-sent: pkt 0x{packet_num:x}");
                    }
                    DiagnosticEvent::FinReceived { packet_num } => {
                        eprintln!("[{peer_tag}] fin-recv: pkt 0x{packet_num:x}");
                    }
                    DiagnosticEvent::FinAckReceived { packet_num } => {
                        eprintln!("[{peer_tag}] fin-ack-recv: pkt 0x{packet_num:x}");
                    }
                }
            }
        }
    };

    // Print the initial prompt. Subsequent prompts are printed by the main
    // loop after all diagnostic output for a send has been flushed.
    let show_prompt = interactive && rx.is_some();
    if show_prompt {
        eprint!("> ");
    }

    // Reason we are leaving the loop. Used so the close()-and-cleanup
    // tail can produce a sensible log line and we don't have three
    // copies of the exit code.
    let exit_reason: &str;

    loop {
        // Always drain diagnostic events so async writer events aren't lost.
        drain_diag(&conn, &peer_tag);

        // Short-circuit if shutdown was requested between iterations.
        if *shutdown.borrow() {
            exit_reason = "shutdown signal";
            break;
        }

        // Drain any pending stdin data into the connection.
        let mut did_send = false;
        let mut stdin_eof = false;
        if let Some(ref mut receiver) = rx {
            loop {
                match receiver.try_recv() {
                    Ok(data) => {
                        did_send = true;
                        let len = data.len();
                        if let Err(e) = conn.send(&data) {
                            eprintln!("send error: {e:?}");
                            return Err(e);
                        }
                        total_sent += len;
                        if diag() {
                            eprintln!("[{peer_tag}] sent {len} bytes, total {total_sent}");
                            hex_dump(&format!("  [{peer_tag}] >>>"), &data);
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        stdin_eof = true;
                        break;
                    }
                }
            }
            if did_send {
                // Drain again to catch data-sent events that arrived during send.
                drain_diag(&conn, &peer_tag);
                // Re-print the prompt after all diagnostic output is done.
                if show_prompt {
                    eprint!("> ");
                }
            }
        }
        if stdin_eof {
            // Stdin is gone (Ctrl-D in interactive mode, pipe EOF
            // otherwise). Flush any in-flight sends so the peer has
            // them before we proceed.
            if let Err(e) = conn.flush().await {
                eprintln!("[{peer_tag}] flush after stdin EOF returned {e:?}");
            }
            if close_on_stdin_eof && !interactive {
                // Piped/non-tty stdin in connect mode (e.g. `echo foo |
                // bluefintool host port`): mirror `nc -N` and close the
                // whole connection once the input is drained.
                exit_reason = "stdin EOF";
                break;
            } else {
                // Either interactive (tty) or a mode where we shouldn't
                // tear down on stdin closure (e.g. the listener whose
                // stdin may be `/dev/null` or a one-line script). Drop
                // the stdin receiver and keep the recv loop alive until
                // the peer FINs us or the user hits Ctrl-C.
                eprintln!(
                    "[{peer_tag}] stdin EOF — half-closing (recv side stays open; Ctrl-C to exit)"
                );
                rx = None;
            }
        }

        // Receive from the peer with a short timeout so we loop back to
        // check stdin regularly. 100 ms is imperceptible for interactive use.
        // Race the recv against the shutdown watch so Ctrl-C is observed
        // without waiting for the next 100 ms tick.
        let buf_len = buf.len();
        let recv_outcome = tokio::select! {
            biased;
            res = shutdown.changed() => {
                // Sender dropped or value flipped: in either case we want to exit.
                let _ = res;
                Err("shutdown")
            }
            res = tokio::time::timeout(Duration::from_millis(100), conn.recv(&mut buf, buf_len)) => {
                Ok(res)
            }
        };
        match recv_outcome {
            Err(_shutdown_marker) => {
                exit_reason = "shutdown signal";
                break;
            }
            Ok(Ok(Ok(0))) => {
                // Bluefin protocol §10bis: `recv` returning Ok(0) signals
                // that the peer sent a `Fin` and the local data buffer is
                // drained. Treat as graceful peer-initiated close.
                exit_reason = "peer closed (EOF)";
                break;
            }
            Ok(Ok(Ok(n))) => {
                // Drain diag FIRST so data-recv packet numbers appear
                // before the hex dump / payload they describe.
                drain_diag(&conn, &peer_tag);
                total_recv += n;
                if diag() {
                    eprintln!("[{peer_tag}] recv {n} bytes, total {total_recv}");
                    hex_dump(&format!("  [{peer_tag}] <<<"), &buf[..n]);
                } else if interactive {
                    // In non-diag interactive mode, prefix with the peer tag.
                    eprint!("[{peer_tag}] ");
                    stdout.write_all(&buf[..n]).await.ok();
                } else {
                    stdout.write_all(&buf[..n]).await.ok();
                }
                stdout.flush().await.ok();
            }
            Ok(Ok(Err(e))) => {
                eprintln!("recv error: {e:?}");
                return Err(e);
            }
            Ok(Err(_)) => continue, // timeout — loop back to drain stdin
        }
    }

    eprintln!("[{peer_tag}] closing connection ({exit_reason}) ...");
    // One last diag drain so the writer's post-flush `data-sent` events
    // (and any in-flight `ack-recv`s) make it to stderr before we tear
    // the connection down and drop the diag channel.
    drain_diag(&conn, &peer_tag);
    // When the peer initiated the close (we observed their `Fin` via the
    // `recv -> Ok(0)` path) their side is already gone and our auto-
    // FinAck has been sent by the conn_reader drainer task. Calling
    // `close()` ourselves here would only burn the retransmit budget
    // before timing out; skip it.
    let peer_initiated = exit_reason == "peer closed (EOF)";
    if !peer_initiated {
        match conn.close().await {
            Ok(()) => {
                drain_diag(&conn, &peer_tag);
                eprintln!(
                    "[{peer_tag}] connection closed (local Fin acked by peer)"
                );
            }
            Err(e) => {
                eprintln!("[{peer_tag}] close() returned {e:?}");
                eprintln!(
                    "[{peer_tag}] connection closed (Fin sent; no FinAck from peer)"
                );
            }
        }
    } else {
        // Our auto-`FinAck` is sent by a separate drainer task fed via
        // `fin_ack_tx` (see `worker::conn_reader::buffer_in_close_packets`)
        // so the on-wire send — and its `FinAckSent` diag event — race
        // with our `recv -> Ok(0)` wakeup. Give the drainer a short
        // window to run and then drain again so the diag stream reflects
        // what actually went out before we tear the connection down.
        tokio::time::sleep(Duration::from_millis(100)).await;
        drain_diag(&conn, &peer_tag);
        eprintln!("[{peer_tag}] connection closed (peer Fin observed)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Listen mode
// ---------------------------------------------------------------------------

/// Spawns a task that reads stdin line-by-line and sends each line into the
/// returned channel. The task exits on EOF or if the receiver is dropped.
fn spawn_stdin_reader() -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    if interactive {
        eprintln!("Type a line and press Enter to send. Ctrl-C or Ctrl-D to quit.");
    }

    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.as_bytes().to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    rx
}

// ---------------------------------------------------------------------------
// Listen mode
// ---------------------------------------------------------------------------

async fn do_listen(bind: &str, port: u16, shutdown: watch::Receiver<bool>) -> BluefinResult<()> {
    let addr: Ipv4Addr = bind.parse().unwrap_or_else(|_| {
        eprintln!("error: invalid bind address: {bind}");
        std::process::exit(1);
    });
    let sock_addr = SocketAddr::V4(SocketAddrV4::new(addr, port));
    eprintln!("listening on {sock_addr} ...");

    let mut server = BluefinServer::new(sock_addr);
    server.set_num_reader_workers(1)?;
    if diag() {
        let (tx, _rx) = flume::bounded::<DiagnosticEvent>(256);
        server.set_diagnostics(tx);
    }
    server.bind().await?;

    let mut first = true;
    let mut shutdown_for_loop = shutdown.clone();
    loop {
        if *shutdown_for_loop.borrow() {
            eprintln!("shutdown signal received — stopping accept loop");
            return Ok(());
        }
        eprintln!("waiting for connection ...");
        let conn = tokio::select! {
            biased;
            _ = shutdown_for_loop.changed() => {
                eprintln!("shutdown signal received — stopping accept loop");
                return Ok(());
            }
            res = server.accept() => {
                match res {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("accept error: {e:?}");
                        continue;
                    }
                }
            }
        };
        eprintln!(
            "connection established (src_id={:08x}, dst_id={:08x})",
            conn.src_conn_id, conn.dst_conn_id
        );
        if diag() {
            dump_connection_info(&conn, "server (listener)", &sock_addr.to_string(), None);
        }

        // The first connection gets stdin; subsequent connections are
        // receive-only (there's only one stdin).
        let stdin_rx = if first {
            first = false;
            Some(spawn_stdin_reader())
        } else {
            eprintln!("(recv-only — stdin is bound to the first connection)");
            None
        };

        let task_shutdown = shutdown.clone();
        tokio::spawn(async move {
            // Listener: never close on stdin EOF (its stdin is often
            // `Stdio::null` or a one-shot script). Wait for the peer to
            // FIN or for Ctrl-C.
            if let Err(e) = run_pipe(conn, stdin_rx, task_shutdown, false).await {
                eprintln!("pipe error: {e:?}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connect mode
// ---------------------------------------------------------------------------

async fn do_connect(
    host: &str,
    port: u16,
    source_port: u16,
    shutdown: watch::Receiver<bool>,
) -> BluefinResult<()> {
    let dst_addr: Ipv4Addr = host.parse().unwrap_or_else(|_| {
        eprintln!("error: invalid host address: {host}");
        std::process::exit(1);
    });
    let dst = SocketAddr::V4(SocketAddrV4::new(dst_addr, port));
    let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), source_port));

    eprintln!("connecting to {dst} from :{source_port} ...");

    let mut client = BluefinClient::new(src);
    if diag() {
        let (tx, _rx) = flume::bounded::<DiagnosticEvent>(256);
        client.set_diagnostics(tx);
    }
    let conn = client.connect(dst).await?;
    eprintln!(
        "connection established (src_id={:08x}, dst_id={:08x})",
        conn.src_conn_id, conn.dst_conn_id
    );
    if diag() {
        dump_connection_info(&conn, "client", &src.to_string(), Some(&dst.to_string()));
    }

    let stdin_rx = spawn_stdin_reader();
    // Connect mode: piped stdin EOF closes the whole connection so
    // `echo foo | bluefintool host port` terminates after delivering.
    run_pipe(conn, Some(stdin_rx), shutdown, true).await
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let mode = parse_args();

    // Print banner to stderr only when connected to a terminal.
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprintln!(
            r#"
  ██████╗ ██╗     ██╗   ██╗███████╗███████╗██╗███╗   ██╗
  ██╔══██╗██║     ██║   ██║██╔════╝██╔════╝██║████╗  ██║
  ██████╔╝██║     ██║   ██║█████╗  █████╗  ██║██╔██╗ ██║
  ██╔══██╗██║     ██║   ██║██╔══╝  ██╔══╝  ██║██║╚██╗██║
  ██████╔╝███████╗╚██████╔╝███████╗██║     ██║██║ ╚████║
  ╚═════╝ ╚══════╝ ╚═════╝ ╚══════╝╚═╝     ╚═╝╚═╝  ╚═══╝
        "#
        );
        eprintln!("  v{}\n", env!("CARGO_PKG_VERSION"));
    }

    // Single shutdown watch driven by SIGINT (Ctrl-C). Cloned into every
    // spawned pipe task and the accept loop so a single Ctrl-C drains
    // everything via [`BluefinConnection::close`] before the process
    // exits.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_observer = shutdown_rx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n^C received — closing connections gracefully ...");
            let _ = shutdown_tx.send(true);
        }
    });

    let result = match mode {
        Mode::Listen { bind, port } => do_listen(&bind, port, shutdown_rx).await,
        Mode::Connect { host, port, source_port } => {
            do_connect(&host, port, source_port, shutdown_rx).await
        }
    };
    if let Err(e) = result {
        eprintln!("fatal: {e:?}");
        std::process::exit(1);
    }
    // If Ctrl-C drove us here, the `tokio::io::stdin()` reader task is
    // still parked in a blocking `read()` syscall on the tty (tokio's
    // stdin is backed by a blocking thread that can't be cancelled). The
    // runtime would otherwise wait on that thread before exiting, making
    // the process appear to hang until the user hits Enter. Bypass that
    // by `exit`ing explicitly once the workload has finished draining.
    if *shutdown_observer.borrow() {
        std::process::exit(0);
    }
}
