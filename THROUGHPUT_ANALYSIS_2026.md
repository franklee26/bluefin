# Bluefin Throughput Analysis — Future Work

Analysis of the benchmark client/server (`bluefin/src/bin/client.rs`, `bluefin/src/bin/server.rs`) and the codepaths they exercise. Cross-referenced against `OPTIMIZATIONS.md`, `THROUGHPUT_OPTIMIZATIONS.md`, `ADDITIONAL_OPTIMIZATIONS.md`, `TIER_3_OPTIMIZATIONS.md`, `TIER_B_OPTIMIZATIONS.md`, `OPTIMIZATION_SUMMARY.md` so nothing already implemented is listed as new.

Baseline at time of analysis: **~3.03 GB/s per connection** (loopback, single 1500 B payload, macOS).

---

## What the benchmark exercises

`client.rs` opens 2 connections, then loops `conn.send(&[0u8; 1500])` ~10M times with `sleep(1ms)` every 8000 sends. `server.rs` loops `conn.recv(&mut buf, 10000).await`.

Hot paths:

- **Client send**: `BluefinConnection::send` → `WriterHandler::send_data` → mpsc → `read_data` task → packetize → mpsc → sender task → `try_send`.
- **Server recv**: kernel UDP recv → `ConnReaderHandler::tx_impl` (per-conn reader) → mpsc → `rx_impl` → `OrderedBytes` → waker → `ReaderRxChannel::read` → `consume` into user buffer.

---

## High-impact bottlenecks

### 1. `send_data` does `payload.to_vec()` on every send
[`bluefin/src/worker/writer.rs:212`](bluefin/src/worker/writer.rs)

```rust
if sender.send(payload.to_vec()).is_err() { ... }
```

At 1500 B × millions of sends/sec this is a measured hot spot. Already flagged as item #6 in `OPTIMIZATION_SUMMARY.md`, never landed.

**Fix**: change the data channel from `mpsc::UnboundedSender<Vec<u8>>` to `mpsc::UnboundedSender<bytes::Bytes>` (or `Box<[u8]>`). Better still, expose `send_bytes(Bytes)` so callers can hand in something they already own → zero copy.

### 2. `conn_reader::tx_impl` clones the entire packet vector per datagram
[`bluefin/src/worker/conn_reader.rs:121`](bluefin/src/worker/conn_reader.rs)

```rust
let _ = tx.send(packets.clone()).await;
```

Comment claims this is needed because the buffer is reused — but `packets: Vec<BluefinPacket>` doesn't share memory with `buf_storage`; only inner payloads were copied during `from_bytes_into`. So `clone()` fully duplicates every packet's payload (~1500 B each, up to 10/datagram). At 3 GB/s throughput that's ~3 GB/s of pure waste.

**Fix**: `let _ = tx.send(std::mem::take(&mut packets)).await;` then re-allocate (or reuse a small pool of `Vec<BluefinPacket>` carriers). One small alloc per datagram replaces N×1500 B copies.

### 3. `from_bytes_into` still allocates one `Vec<u8>` per packet
[`bluefin/src/core/packet.rs:151-200`](bluefin/src/core/packet.rs)

Parses into a pre-allocated outer Vec but every payload is a fresh `Vec::with_capacity(payload_len) + copy_nonoverlapping`. ~10 allocs per recv'd datagram.

**Fix**: make `BluefinPacket::payload` either:

- `bytes::Bytes` backed by an `Arc<[u8; MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM]>` (the recv buffer becomes a `BytesMut` you `freeze()` after recv, then `slice()` per packet → zero-copy parsing), or
- `&'a [u8]` borrow with parsing returning a temporary view, then copy directly into the connection buffer.

Single change removes per-packet allocs on receive and the carry-over `to_vec`/`split_off` work in `OrderedBytes`.

### 4. `OrderedBytes` stores owned `BluefinPacket`s, then memcpys payload into user buf
[`bluefin/src/net/ordered_bytes.rs`](bluefin/src/net/ordered_bytes.rs)

Holds `[Option<BluefinPacket>; 2000]` and on `consume` does `buf.copy_from_slice(&packet.payload)`. If recv buffers were ref-counted as in (3), `consume` becomes a single bulk memcpy from kernel buffer → user buf with no intermediate Vec.

Also: `#[derive(Clone)]` on `ConnectionBuffer` is suspicious — confirm nothing actually clones it; if not, drop the `Clone` derives so a future change can't accidentally do so.

### 5. Writer pipeline is a two-hop async path
[`bluefin/src/worker/writer.rs:144-180`](bluefin/src/worker/writer.rs)

`read_data` packetizes, then sends each finished datagram to a *second* spawned task via another unbounded mpsc just so that task can call `try_send`/`writable().await`. Adds one channel + one extra task wakeup per datagram. The receiver does nothing but `try_send`, so the "decouple packetization from sending" intent is moot.

**Fix**: inline the send back into `read_data` — `try_send`, on `WouldBlock` do `socket.writable().await` then retry. Same shape as `read_ack` already uses.

### 6. Vectorized I/O is built but unused
[`bluefin-io/src/socket/udp_socket.rs`](bluefin-io/src/socket/udp_socket.rs)

`recvmsg_x`/`sendmsg_x` (macOS) implemented under the `macos-fast` feature, *and* `BluefinSocket` is never wired into the runtime path (only used in `bluefin-io/tests/basic_io.rs`). The actual server uses `tokio::net::UdpSocket::recv_from` one datagram at a time. Switching the hot reader to `recvmmsg`/`recvmsg_x` typically gives 2–5× recv throughput.

Even on the existing `recvmsg_x` path the doc notes the bug: returns `Ok(1)` after receiving up to 8 messages. Fix that, then wire `BluefinSocket` into `ConnReaderHandler::tx_impl`. Highest single-feature lever available.

On Linux the equivalent is `sendmmsg`/`recvmmsg` (kernel-supported, no private API).

### 7. Per-connection reader pool re-binds the socket
[`bluefin/src/net/connection.rs:230`](bluefin/src/net/connection.rs)

`BluefinConnection::new` calls `get_connected_udp_socket`, creating a new connected UDP socket per established connection, then `ConnReaderHandler` spawns N tx tasks (1 on macOS, num_cpus on Linux) each in `socket.recv(buf)`.

- The original demux `ReaderTxChannel` is still running on the bound socket; it's only used during handshake on this new socket because the per-connection socket is `connect()`-ed.
- Multiple reader tasks on the same connected socket race for the same datagram via `recv()` — only one wins. On macOS (1 task) you don't gain anything. On Linux (N tasks) you *can* benefit from kernel-side parallelism but only with `SO_REUSEPORT`. `get_udp_socket_impl` does set it, but `get_connected_udp_socket` then `connect()`s it which serializes recvs on a single FD again.

**Fix**: drop `ConnReaderHandler` for the connected client/server case and reuse the demux reader (`ReaderTxChannel`), which already routes packets to the right `ConnectionManagedBuffers` via the lock-free `DashMap`. One reader path, one socket per host endpoint.

---

## Medium-impact wins

### 8. `Mutex<ConnectionBuffer>` in the recv path
Every received datagram requires `buffers.conn_buff.lock()`; every user `recv()` needs the same lock. The waker `wake_by_ref()` is called *while holding the lock* in `ConnReaderHandler::buffer_in_data_packets` — the woken task immediately tries to grab it and bounces.

**Fix**: switch to `parking_lot::Mutex` (already proposed in `ADDITIONAL_OPTIMIZATIONS.md` Phase 2, never landed). Wake *after* dropping the guard.

### 9. `SlidingWindow::insert_packet_number` is per-packet for ack receipt
[`bluefin/src/net/ack_handler.rs:34`](bluefin/src/net/ack_handler.rs)

```rust
for ix in 0..num_packets_to_ack { insert(base + ix) }
```

For an ack covering 200 packets that's 200 VecDeque insertions.

**Fix**: add `SlidingWindow::insert_range(base, count)` that does a single contiguous range insertion (or just bumps `smallest_expected_packet_number` directly when the range starts at the smallest expected value, the common in-order case).

### 10. `consume_data_into` chains many `split_off`s
[`bluefin/src/worker/writer.rs`](bluefin/src/worker/writer.rs) (around line 286)

`running_payload = running_payload.split_off(N)` is O(remaining) (allocates new Vec, memcpys tail). The merge path (`extend` into `running_payload` then `split_off`) ends up copying the same bytes twice (caller→running_payload→datagram).

**Fixes**:

- Replace `running_payload: Vec<u8>` with a `(Vec<u8>, cursor: usize)` slice view: consume from `&running_payload[cursor..]`, deallocate only when fully drained. Eliminates split_off copies in the merge case.
- For the single-packet case (the benchmark: 1500 B == `MAX_BLUEFIN_PAYLOAD_SIZE_BYTES`), build header + write payload bytes straight into the datagram, skip the intermediate `running_payload` buffer entirely.

### 11. UnboundedSender on the data path
The data channel is `mpsc::unbounded_channel`. Under burst the writer task keeps allocating queue nodes; the producer never blocks → memory grows during transient stalls and tail latency rises.

**Fix**: use `tokio::sync::mpsc::channel(N)` with a generous bound (e.g. 4096) and `try_send` from `send_data` with a fallback path. Caps memory and lets the caller observe pressure. Better yet for SPSC: `crossbeam::queue::ArrayQueue` + `Notify`.

---

## Low-impact / cleanup

### 12. `AckConsumer` is dead weight
[`bluefin/src/net/ack_handler.rs`](bluefin/src/net/ack_handler.rs)

Wakes on every ack batch just to do `AtomicU64::store` of `largest_recv_acked_packet_num`. Search confirms zero `load` callers. The whole task — and the spawn in `BluefinConnection::new` — is dead weight. Either implement retransmission (presumably motivated it) or remove it.

### 13. Server recv loop does `try_recv_from` then `recv_from`
[`bluefin/src/worker/reader.rs:248-253`](bluefin/src/worker/reader.rs)

On macOS `try_recv_from` almost always fails (the socket is rarely "already" readable when you race to it), so this just adds an extra syscall per packet. Profile this; if it's failing > 90% of the time, drop it.

### 14. `packets_consumed_before_ack = 200` hard-coded
[`bluefin/src/worker/reader.rs:79`](bluefin/src/worker/reader.rs)

Increase to e.g. 500 for throughput-only scenarios to reduce reverse-direction work, or expose as a runtime knob.

---

## Ordered recommended PRs

| # | Change | Risk | Rough gain |
|---|--------|------|------------|
| 1 | Fix `packets.clone()` in `conn_reader::tx_impl` | Low | High (eliminates ~3 GB/s of mem copies) |
| 2 | `payload.to_vec()` → `Bytes` channel | Low | High |
| 3 | Inline send into `read_data` (drop second hop) | Low | Medium |
| 4 | `BluefinPacket::payload: Bytes` backed by `BytesMut` recv buffer | Medium | High |
| 5 | Wire `BluefinSocket` (`recvmsg_x`/`recvmmsg`) into `ConnReaderHandler::tx_impl`; fix `Ok(1)` bug | Medium | Highest single lever |
| 6 | Drop `ConnReaderHandler` per-conn socket; use demux reader | Medium | Architectural; biggest multi-conn win |
| 7 | `parking_lot::Mutex` + wake outside lock | Low | Medium |
| 8 | `SlidingWindow::insert_range` | Low | Medium |
| 9 | `(Vec, cursor)` view in `consume_data_into` | Low | Medium |
| 10 | Bounded mpsc on data channel | Low | Latency / robustness |
| 11 | Remove dead `AckConsumer` | Low | Cleanup |
| 12 | Drop `try_recv_from` on macOS | Trivial | Tiny |
| 13 | Make `packets_consumed_before_ack` configurable | Trivial | Tunability |

---

## What the benchmark hides

- **1500 B sends fit in exactly one packet/datagram**, so the merge path in `consume_data_into` is never exercised. Production payloads of 3–5 KB will hit it hard.
- **Both endpoints on `127.0.0.1`** so `SO_RCVBUF`/`SO_SNDBUF` rarely fill; receiver-side batching wins (item 5) won't show up much on loopback. Test over a real NIC (or `lo` with `tc qdisc add … netem rate`) to see them.
- **Only 2 connections** — `DashMap`/`ConnectionManager` contention is invisible. With many connections, `ReaderTxChannel` does `.clone()` of the `ConnectionManagedBuffers` (Arc clones) for every datagram. Intern these or re-key by raw `*const` for cheaper lookup.

---

## What NOT to retry (already shown to regress)

From `TIER_B_OPTIMIZATIONS.md`:

- `mem::replace` for datagram clone (-5.5%)
- `split_off` in `OrderedBytes::consume` carry-over (-2%)
- Unsafe copy micro-optimizations on small slices (regressed)
- Batch size 32 in writer (regressed to 2.46 GB/s)

Trust the compiler: `drain().collect()` and `copy_from_slice` are well-optimized; don't replace them without a measurement.

---

*Created 2026-05-09. Branch: `frank/cleanup`.*
