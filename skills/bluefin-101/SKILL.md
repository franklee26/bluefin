---
name: bluefin-101
description: Mental model for the Bluefin transport-layer protocol implementation. Covers the workspace layout, the wire format, the connection lifecycle (handshake → data → ack), and the async task topology (reader / writer / conn_reader workers, buffers, wakers). Load this before reading or modifying anything in `bluefin/src/net`, `bluefin/src/worker`, `bluefin/src/core`, or `bluefin-io/src`. Skip for purely external/build/dependency questions.
---

# Bluefin 101

Bluefin is an experimental, secure, P2P, transport-layer protocol on top of UDP. This skill gives you the mental model needed to navigate the code.

## Workspace layout

Three crates in a Cargo workspace (`Cargo.toml` at root):

| Crate | Purpose |
|-------|---------|
| `bluefin/` | Public library + the `client`/`server` benchmark binaries. Owns connection management, worker tasks, ordered byte buffers. |
| `bluefin-io/` | Lower-level UDP socket abstraction. Optional `recvmsg_x`/`sendmsg_x` (macOS) and `recvmmsg`/`sendmmsg` (Linux) wrappers behind the `macos-fast` feature. **Currently NOT wired into the runtime path** (only used by tests). |
| `bluefin-proto/` | Protocol-level error types, `BluefinHost` enum (Client/PackLeader), handshake state machine. |

Benchmark binaries: [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs), [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs).

## Wire format

`BluefinHeader` is exactly 20 bytes ([`bluefin/src/core/header.rs`](../../bluefin/src/core/header.rs)):

```
0               1               2               3
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|Version|  Type |         Type payload          |E|    Mask     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                   Source connection id (u32)                   |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                Destination connection id (u32)                 |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Packet number (u64)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

`PacketType`: `UnencryptedClientHello | UnencryptedServerHello | ClientAck | UnencryptedData | Ack`.

For data packets, `type_specific_payload` = payload byte length. For acks, `packet_number` = base packet number being acked, `type_specific_payload` = number of contiguous packets acked.

## Packet vs Datagram

[`bluefin/src/net/mod.rs`](../../bluefin/src/net/mod.rs):

- `MAX_BLUEFIN_PAYLOAD_SIZE_BYTES = 1500`
- `MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM = 10`
- `MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM = 10 * (20 + 1500) = 15200`

One UDP datagram carries 1..N Bluefin packets, **all for the same connection** and **all the same kind** (data XOR ack). Handshake packets are always 1-per-datagram. These invariants are relied on in `ConnReaderHandler::buffer_in_packets`.

## Connection lifecycle

Three-way handshake driven by `bluefin-proto/src/handshake/state_machine.rs`:

1. Client → Server: `UnencryptedClientHello` (src_conn_id chosen by client, dst=0)
2. Server → Client: `UnencryptedServerHello` (src=server's chosen id, dst=client's id)
3. Client → Server: `ClientAck`
4. Both sides instantiate a `BluefinConnection` and the bidirectional data path is open.

Server entry: [`BluefinServer::accept`](../../bluefin/src/net/server.rs).
Client entry: [`BluefinClient::connect`](../../bluefin/src/net/client.rs).

> **Known live bug**: when two clients hello within ~100 ms of each other, the server can drop the second hello before its `accept()` slot is wired up. Client side surfaces as `TimedOut("Failed to read from handshake connection buffer")` after 3 s. Mitigated by ≥500 ms inter-client stagger; properly fixing it requires queueing parsed hellos in [`net/server.rs`](../../bluefin/src/net/server.rs) until `accept()` is called. See live bottleneck #11 in the [performance skill](../bluefin-performance/SKILL.md) and historical context in [`docs/archive/BINARY_RACE_CONDITIONS.md`](../../docs/archive/BINARY_RACE_CONDITIONS.md).

## The buffer types

Per connection, there are two shared buffers, both `Arc<Mutex<...>>`, bundled as `ConnectionManagedBuffers`:

| Buffer | Contains | Producer | Consumer |
|--------|----------|----------|----------|
| `ConnectionBuffer` ([`net/connection.rs`](../../bluefin/src/net/connection.rs)) | Ordered data bytes (via inner `OrderedBytes`) + handshake packet slot + addr + waker | `ConnReaderHandler` (data) / `ReaderTxChannel` (handshake) | `ReaderRxChannel::read` (data) / `HandshakeConnectionBuffer` (handshake) |
| `AckBuffer` ([`net/ack_handler.rs`](../../bluefin/src/net/ack_handler.rs)) | A `SlidingWindow` of received ack packet numbers + waker | `ConnReaderHandler` (ack packets) | `AckConsumer` (currently a no-op consumer) |

`OrderedBytes` ([`net/ordered_bytes.rs`](../../bluefin/src/net/ordered_bytes.rs)) is a circular array of `Option<BluefinPacket>` indexed by packet number, with a `carry_over_bytes: Option<Vec<u8>>` for partial consumes.

`ConnectionManager = dashmap::DashMap<(src_conn_id, dst_conn_id), ConnectionManagedBuffers>` — lock-free routing from packet → buffer.

## Task topology

Each *host* (server or client process) runs:

- **One `ReaderTxChannel` per `num_reader_workers`** ([`worker/reader.rs`](../../bluefin/src/worker/reader.rs)) bound to the listening socket. Demuxes datagrams to the right `ConnectionManagedBuffers` via `ConnectionManager`. Used for handshake on every host; also used for data when `BluefinConnection`'s per-connection socket isn't taking over.

Each *connection* additionally spawns:

- **One `ConnReaderHandler`** ([`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs)) on a *new* connected UDP socket. Spawns N tx tasks (1 on macOS, num_cpus on Linux) that `recv` and forward parsed packets via mpsc to one `rx_impl` task that buffers them.
- **One `WriterHandler`** ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs)) with two children:
  - `read_data` task: receives `Bytes` payloads from user, packetizes into datagrams, hands off to a *third* spawned task that does the actual `socket.try_send`. The user-facing channel is `mpsc::channel(4096)` (bounded — ~6 MiB cap, ~since 2026-05/#10); enqueueing carries an owned `Bytes` (refcount bump, not a copy, ~since 2026-05/#2).
  - `read_ack` task: receives `AckData` records, builds ack-only datagrams, sends directly with `try_send` (no second hop). Ack channel is unbounded — low-volume, no need to add a new failure mode.
- **One `AckConsumer`** ([`net/ack_handler.rs`](../../bluefin/src/net/ack_handler.rs)) that wakes on every ack batch and writes `largest_recv_acked_packet_num` (currently no readers — dead code, kept for future retransmission).

## Public API surface

```rust
BluefinClient::new(src) → connect(dst).await → BluefinConnection
BluefinServer::new(src) → bind().await → loop { accept().await → BluefinConnection }

BluefinConnection {
    // Borrow-and-copy. Allocates + memcpys once into a fresh `Bytes`.
    fn send(&mut self, &[u8]) -> BluefinResult<usize>

    // Hand-off ownership. Cheap (refcount bump if the caller holds a `Bytes`
    // already). Sync: returns `WriteError` if the bounded send queue is full.
    fn send_bytes(&mut self, Bytes) -> BluefinResult<usize>

    // Same as `send_bytes` but awaits backpressure when the queue is full.
    // Preferred for high-throughput producers.
    async fn send_bytes_async(&mut self, Bytes) -> BluefinResult<usize>

    async fn recv(&mut self, &mut [u8], len: usize) -> BluefinResult<usize>
}
```

The sync `send`/`send_bytes` variants enqueue into the writer's bounded
`mpsc::Sender<Bytes>` (cap 4096) and return immediately; the actual UDP send
happens later on a worker task. If the queue is full (slow consumer or stalled
socket), they return `BluefinError::WriteError`. The async `send_bytes_async`
awaits the channel slot instead of failing — use it on the hot path of bursty
producers so the producer naturally synchronises with the writer's drain rate.

## Wakers

`ConnectionBuffer` and `AckBuffer` each store an `Option<Waker>`. Every poll calls `set_waker_if_changed` ([`net/connection.rs`](../../bluefin/src/net/connection.rs)) which compares with `Waker::will_wake` to avoid cloning when the same task re-polls.

The producer side (the worker task that buffers in a packet) wakes the consumer via the buffer's `take_waker_clone()` helper: clone the `Waker` out (cheap, atomic refcount), `drop(guard)`, then call `wake()`. **Never call `wake_by_ref()` while still holding the buffer's mutex** — the woken consumer immediately tries to `lock()` it and bounces. This was a real bug fixed in 2026-05/#7; if you find yourself adding a new producer-side site, follow the same pattern.

## Reading order for new contributors

1. [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs) + [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs) — what the public API looks like in use.
2. [`bluefin/src/core/header.rs`](../../bluefin/src/core/header.rs) + [`bluefin/src/core/packet.rs`](../../bluefin/src/core/packet.rs) — the wire format.
3. [`bluefin/src/net/connection.rs`](../../bluefin/src/net/connection.rs) — what a connection actually owns.
4. [`bluefin/src/net/ordered_bytes.rs`](../../bluefin/src/net/ordered_bytes.rs) — receive-side ordering.
5. [`bluefin/src/worker/reader.rs`](../../bluefin/src/worker/reader.rs) — handshake/demux read path.
6. [`bluefin/src/worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) — per-connection read path.
7. [`bluefin/src/worker/writer.rs`](../../bluefin/src/worker/writer.rs) — packetization + send path.

## Running the benchmark

Use [`bench_two_process.sh`](../../bench_two_process.sh) — it builds release, kills stale processes, spawns one server and N client processes (each invoked with `--task <ix>` to run a single connection), waits for the server's 2 s idle-timeout exit, and prints a summary. Defaults to 2 connections, 0.5 s inter-client stagger (works around the handshake race noted above), and up to 2 auto-retries.

```bash
./bench_two_process.sh                  # 2 connections
./bench_two_process.sh -n 5             # 5 connections
./bench_two_process.sh --skip-build     # skip cargo build
./bench_two_process.sh --help
```

Per-attempt logs land in `bench_logs/<timestamp>/attempt_<N>/`. See the [performance skill](../bluefin-performance/SKILL.md) for what to read in those logs and what numbers count as good.

## Conventions in this codebase

- All public-API errors return `BluefinResult<T> = Result<T, BluefinError>` from `bluefin-proto`.
- Hot-path methods are aggressively `#[inline]`/`#[inline(always)]`.
- `unsafe` is used in serialization paths; preconditions are documented above each block. Don't add new unsafe without a measurable win.
- Tests live next to the code they exercise (`#[cfg(test)] mod tests`) plus `bluefin/tests/` for integration. There's also `kani` proof code under `#[cfg(kani)]` for the writer queue.
- `#[cfg(coverage_nightly)]` attributes opt files out of coverage instrumentation.
