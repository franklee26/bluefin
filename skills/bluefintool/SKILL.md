# bluefintool — netcat for Bluefin

Quick-and-dirty interactive tool for the Bluefin protocol. Modelled after `nc`
(netcat): one side listens, the other connects; stdin flows to the peer, peer
data flows to stdout.

## When to load

Any time you want to:

* smoke-test that two Bluefin endpoints can handshake and exchange bytes
* interactively send/receive data over a Bluefin connection
* pipe data through a Bluefin tunnel (e.g. `cat file | bluefintool …`)

This is **not** the benchmarking tool — use `bench_two_process.sh` and the
`client`/`server` binaries for throughput measurement (see
[bluefin-performance](../bluefin-performance/SKILL.md)).

## Install

```bash
cargo install --path bluefintool
```

Installs to `~/.cargo/bin/` (usually on `$PATH`). Or build without installing:

## Building

```bash
cargo build --release --bin bluefintool
```

Binary lands at `target/release/bluefintool`.

## Usage

```
bluefintool — netcat-like tool for the Bluefin protocol

USAGE:
  bluefintool -l <port>                     Listen on 127.0.0.1:<port>
  bluefintool -l <port> -b <bind_addr>      Listen on <bind_addr>:<port>
  bluefintool <host> <port>                 Connect to <host>:<port>
  bluefintool <host> <port> -s <src_port>   Connect from specific source port

OPTIONS:
  -l, --listen               Listen mode (accept multiple connections)
  -b, --bind <addr>          Bind address for listen mode [default: 127.0.0.1]
  -s, --source-port <port>   Source port for connect mode [default: random]
  -d, --diagnostics          Show hex dumps, byte counts, packet numbers, and ACK events
  -h, --help                 Show this help
```

### Listen mode (server side)

```bash
# Terminal 1 — listen on port 9000
./target/release/bluefintool -l 9000
```

Accepts connections in a loop. The first client gets the stdin pipe
(bidirectional); subsequent clients are receive-only. All received data is
prefixed with the peer's connection ID so you can tell connections apart:

```
[03d1b5dc] hello from client 1
[4d7d12a6] hello from client 2
```

Ctrl-C to quit. The listener calls [`BluefinConnection::close`](../../bluefin/src/net/connection.rs) on
every active connection so the peer observes a clean `Fin` and exits its
own `recv` with EOF. Ctrl-D on the listener's own stdin is **not** treated
as exit — the listener half-closes its send side and keeps recv'ing
(matches `nc -l`); this also means piping `Stdio::null()` into a listener
in tests does not collapse the accept loop.

### Connect mode (client side)

```bash
# Terminal 2 — connect to the listener
./target/release/bluefintool 127.0.0.1 9000
```

After the 3-way handshake completes you can type lines into either terminal and
see them appear in the other.

#### Exit semantics

| Trigger | Behaviour |
|---------|-----------|
| Ctrl-C (SIGINT) | Calls `close()` on the active connection then exits. Peer's `recv` returns EOF. |
| Ctrl-D on a tty stdin | Half-closes the send side. Keeps recv'ing until peer FINs or Ctrl-C. |
| Pipe EOF in **connect mode** (`echo … \| bluefintool host port`) | Flushes, calls `close()`, exits — matches `nc -N`. |
| Pipe EOF in **listen mode** | Treated as Ctrl-D (half-close). |
| Peer FIN received (`recv` returns `Ok(0)`) | Logs `closing connection (peer closed (EOF))` and exits without re-sending FIN (the peer is already gone). |

### Diagnostics mode (`-d`)

```bash
./target/release/bluefintool -l 9000 -d
./target/release/bluefintool 127.0.0.1 9000 -d
```

Shows running byte counters, hex dumps (with ASCII sidebar), packet sequence
numbers (in hex), ACK events, and a connection info dump on handshake.
In diagnostics mode, received data is shown only in the hex dump (no
duplicate plain-text echo). Four event types:

| Event | Meaning |
|-------|---------|
| `data-sent` | Writer packetized and queued bytes for sending (wire view) |
| `data-recv` | Packets consumed from the receive buffer |
| `ack-sent` | We acknowledged the peer's data (~every 200 packets) |
| `ack-recv` | Peer acknowledged data we sent |

Example (server side):

```
[651feb54] data-recv: pkts 0x929a1f3b8e4c655d..+3 (16 bytes)
[651feb54] recv 16 bytes, total 16
  [651feb54] <<< 0000  68 65 6c 6c 6f 0a 77 6f 72 6c 64 0a 66 6f 6f 0a   |hello.world.foo.|
```

Connection info dump on handshake:

```
connection established (src_id=304d173d, dst_id=b3b69e91)
  local_addr  = 127.0.0.1:9040
  remote_addr = 127.0.0.1:9000
  src_conn_id = 0x304d173d
  dst_conn_id = 0xb3b69e91
  role        = client
  version     = 0x0
  encrypted   = false
  mask        = 0x00
```

Example (client side):

```
[af6ca19e] sent 6 bytes, total 6
  [af6ca19e] >>> 0000  68 65 6c 6c 6f 0a                                 |hello.|
[af6ca19e] data-sent: pkts 0x929a1f3b8e4c655d..+1 (6 bytes)
```

**Pipeline note:** `sent X bytes` = application enqueued X bytes (immediate).
`data-sent` = writer task serialized X bytes into Bluefin packet(s)
(asynchronous). The writer may merge small sends into one packet or split
large payloads across multiple packets.

### Piping data

```bash
# Send a file over Bluefin
cat payload.bin | ./target/release/bluefintool 127.0.0.1 9000

# Receive into a file
./target/release/bluefintool -l 9000 > received.bin
```

### Quick connectivity check

```bash
# One-liner: send a greeting and exit
echo "hello" | ./target/release/bluefintool 127.0.0.1 9000
```

## Architecture

```
stdin ─► [mpsc channel] ─► conn.send() ─────► peer
                                                │
stdout ◄──────────────── conn.recv() ◄──────────┘
```

* A dedicated tokio task reads stdin line-by-line into a bounded mpsc channel.
* The main loop alternates between draining the channel (send) and receiving
  from the peer with a 100 ms timeout, so neither direction starves the other.
* All status/diagnostic output goes to **stderr**; only peer data goes to
  **stdout**, so pipes work correctly.
* In listen mode the accept loop runs forever; each connection is spawned as
  its own task. The first gets stdin, the rest are recv-only.
* Received data is tagged with the peer's `dst_conn_id` (8-hex-char tag) in
  interactive mode so you can identify connections.

## Source

* Cargo project: [`bluefintool/`](../../bluefintool/)
* Single binary: [`bluefintool/src/main.rs`](../../bluefintool/src/main.rs)

## Differences from the bench binaries

| | `bluefintool` | `client` / `server` bins |
|---|---|---|
| Purpose | Interactive / smoke-test | Throughput benchmarking |
| Connections | N (first gets stdin, rest recv-only) | N (configurable) |
| Data source | stdin | Fixed 1500 B payload × millions |
| Output | Peer data to stdout (tagged) | Throughput stats to stderr |
| Reader workers | 1 | Configurable (default 3) |
| Idle timeout | 100 ms poll loop | Coarse `Sleep` reset (default 2 s) |
| Diagnostics | `-d` flag: hex dumps, packet numbers, ACK events | Built-in throughput stats |
