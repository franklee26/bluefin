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
async fn run_pipe(
    mut conn: bluefin::net::connection::BluefinConnection,
    stdin_rx: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
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
                }
            }
        }
    };

    loop {
        // Drain any pending stdin data into the connection.
        if let Some(ref mut receiver) = rx {
            while let Ok(data) = receiver.try_recv() {
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
            // Show data-sent packet numbers right after the send output.
            drain_diag(&conn, &peer_tag);
        }

        // Receive from the peer with a short timeout so we loop back to
        // check stdin regularly. 100 ms is imperceptible for interactive use.
        let buf_len = buf.len();
        match tokio::time::timeout(Duration::from_millis(100), conn.recv(&mut buf, buf_len))
            .await
        {
            Ok(Ok(n)) if n > 0 => {
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
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => {
                eprintln!("recv error: {e:?}");
                return Err(e);
            }
            Err(_) => continue, // timeout — loop back to drain stdin
        }
    }
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
            if interactive {
                eprint!("> ");
            }
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

async fn do_listen(bind: &str, port: u16) -> BluefinResult<()> {
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
    loop {
        eprintln!("waiting for connection ...");
        let conn = match server.accept().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("accept error: {e:?}");
                continue;
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

        tokio::spawn(async move {
            if let Err(e) = run_pipe(conn, stdin_rx).await {
                eprintln!("pipe error: {e:?}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connect mode
// ---------------------------------------------------------------------------

async fn do_connect(host: &str, port: u16, source_port: u16) -> BluefinResult<()> {
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
    run_pipe(conn, Some(stdin_rx)).await
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let mode = parse_args();

    // Print banner to stderr only when connected to a terminal.
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprintln!(r#"
     ___  __         _____
    / _ )/ /_ _____ / __(_)__
   / _  / / // / -_) _// / _ \
  /____/_/\_,_/\__/_/ /_/_//_/
        "#);
    }

    let result = match mode {
        Mode::Listen { bind, port } => do_listen(&bind, port).await,
        Mode::Connect { host, port, source_port } => do_connect(&host, port, source_port).await,
    };
    if let Err(e) = result {
        eprintln!("fatal: {e:?}");
        std::process::exit(1);
    }
}
