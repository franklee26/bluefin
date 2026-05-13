# bluefintool

A netcat-like CLI for the [Bluefin](../README.md) protocol. Quickly spin up a
listener and client to verify connectivity and send a few bytes — without
touching the bench harness.

## Install

```bash
cargo install --path bluefintool
```

This compiles an optimised binary and copies it to `~/.cargo/bin/`, which is
normally already on your `$PATH`. After that you can run `bluefintool` from
anywhere.

To build without installing:

```bash
cargo build --release --bin bluefintool
# binary at target/release/bluefintool
```

## Usage

```
bluefintool -l <port>                     Listen on 127.0.0.1:<port>
bluefintool -l <port> -b <bind_addr>      Listen on <bind_addr>:<port>
bluefintool <host> <port>                 Connect to <host>:<port>
bluefintool <host> <port> -s <src_port>   Connect from specific source port
```

| Flag | Description |
|------|-------------|
| `-l`, `--listen` | Listen mode (accept multiple connections) |
| `-b`, `--bind <addr>` | Bind address for listen mode [default: 127.0.0.1] |
| `-s`, `--source-port <port>` | Source port for connect mode [default: random] |
| `-d`, `--diagnostics` | Show hex dumps, byte counts, packet sequence numbers, and ACK events |
| `-h`, `--help` | Show this help |

### Interactive session

```bash
# Terminal 1 — listen
bluefintool -l 9000

# Terminal 2 — connect
bluefintool 127.0.0.1 9000
```

Type into either terminal and see the text appear in the other. Received data
is prefixed with the peer's connection ID (e.g. `[a1b2c3d4]`) so you can tell
which client it came from. Ctrl-C or Ctrl-D to quit.

### Multiple clients

The listener accepts connections in a loop. The first client gets the stdin
pipe (bidirectional); subsequent clients are receive-only. All received data
is tagged with the peer ID:

```
[03d1b5dc] hello from client 1
[4d7d12a6] hello from client 2
```

### Diagnostics mode

```bash
bluefintool -l 9000 -d
bluefintool 127.0.0.1 9000 -d
```

Shows byte counts, hex dumps, packet sequence numbers (in hex), ACK
events, and a connection info dump on handshake. In diagnostics mode,
received data is shown only in the hex dump (no duplicate plain-text echo).

Four event types are reported:

| Event | Meaning |
|-------|---------|
| `data-sent` | Writer packetized and queued bytes for sending |
| `data-recv` | Packets consumed from the receive buffer |
| `ack-sent` | We sent an ACK to the peer (every ~200 packets received) |
| `ack-recv` | Peer acknowledged packets we sent |

Example client output:

```
[e9aee750] sent 6 bytes, total 6
  [e9aee750] >>> 0000  68 65 6c 6c 6f 0a                                 |hello.|
[e9aee750] data-sent: pkts 0xa6e12db6165b9818..+1 (6 bytes)
```

Example server output:

```
[651feb54] data-recv: pkts 0xa6e12db6165b9818..+3 (16 bytes)
[651feb54] recv 16 bytes, total 16
  [651feb54] <<< 0000  68 65 6c 6c 6f 0a 77 6f 72 6c 64 0a 66 6f 6f 0a   |hello.world.foo.|
```

Connection info is dumped on handshake:

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

**Note:** `sent X bytes` and `data-sent` are two different pipeline stages.
`send()` enqueues bytes immediately (application view); the writer task
packetizes them asynchronously (wire view), potentially merging small sends
into one packet or splitting large ones across multiple packets.

### Pipe data

```bash
# Send a file
cat payload.bin | bluefintool 127.0.0.1 9000

# Receive into a file
bluefintool -l 9000 > received.bin

# Quick connectivity check
echo "hello" | bluefintool 127.0.0.1 9000
```

All diagnostics go to stderr; only peer data goes to stdout, so pipes work
correctly.
