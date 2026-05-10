---
name: bluefin-performance
description: Performance and throughput rules for the Bluefin codebase. Captures the hot paths, the current baseline (~3.03 GB/s loopback), what's been tried, what's been proven to regress, where the live bottlenecks are, and how to benchmark/profile correctly. Load this in addition to `bluefin-101` whenever a task touches the worker, net, ordered_bytes, ack_handler, packet, or socket layers, or whenever the user says "throughput", "latency", "allocations", "hot path", "slow", or "optimize". Skip for pure correctness/feature work that is clearly not perf-sensitive.
---

# Bluefin Performance

This skill is the consolidated record of every perf-related decision in the codebase. It supersedes the older root-level docs (now under [`docs/archive/`](../../docs/archive/)). The forward-looking backlog lives separately in [`THROUGHPUT_ANALYSIS_2026.md`](../../THROUGHPUT_ANALYSIS_2026.md).

## Baseline & topology

- **Current**: ~3.03 GB/s per connection on loopback (macOS, single 1500 B payload). Up from 2.5 GB/s baseline (+21.6%).
- **Benchmarks**: [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs) + [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs). Loopback only, single 1500 B payload, 2 connections.
- **Release profile** (root `Cargo.toml`): `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `debug = true`.

## The hot paths (memorize these)

### Send (client side)
```
user code
  → BluefinConnection::send                                    (sync)
  → WriterHandler::send_data                                   (sync; ALLOCATES via payload.to_vec())
  → mpsc::UnboundedSender<Vec<u8>>                             (cross-task wakeup)
  → WriterHandler::read_data task                              (packetize)
  → mpsc::UnboundedSender<Vec<u8>>                             (cross-task wakeup, second hop!)
  → spawned sender task
  → socket.try_send                                            (syscall)
```

### Recv (server side)
```
kernel UDP queue
  → ConnReaderHandler::tx_impl                                 (one or more tasks)
  → BluefinPacket::from_bytes_into                             (ALLOCATES one Vec per packet)
  → mpsc::Sender<Vec<BluefinPacket>>                           (mem::take, no clone since 2026-05/#1)
  → ConnReaderHandler::rx_impl
  → buffers.conn_buff.lock()                                   (Mutex)
  → OrderedBytes::buffer_in_packet
  → guard.take_waker_clone(); drop(guard); waker.wake()        (wake AFTER drop since 2026-05/#7)
  → ReaderRxChannelFuture::poll → ReaderRxChannel::read
  → buffers.conn_buff.lock()                                   (Mutex again)
  → OrderedBytes::consume → buf.copy_from_slice(payload)       (memcpy)
  → user code
```

## Live bottlenecks (high → low impact)

Full prioritized list lives in [`THROUGHPUT_ANALYSIS_2026.md`](../../THROUGHPUT_ANALYSIS_2026.md). Headlines:

1. ~~**`tx.send(packets.clone())`**~~ — **DONE 2026-05 (#1).** `mem::take(&mut packets)` + new `Vec::with_capacity(76)` per datagram in [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs). No measurable Δ on send-bound bench but kept (correct shape, recv-side win).
2. ~~**`payload.to_vec()`**~~ — **DONE 2026-05 (#2, +5.6%).** Writer's data mpsc carries `bytes::Bytes`; new `BluefinConnection::send_bytes(Bytes)` for callers that already own a buffer. ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs), [`net/connection.rs`](../../bluefin/src/net/connection.rs))
3. ~~**`from_bytes_into` allocates one `Vec<u8>` per packet**~~ — **DONE 2026-05 (E).** [`core/packet.rs`](../../bluefin/src/core/packet.rs) now stores `payload: Bytes` and the hot recv path takes a `Bytes` argument: `BluefinPacket::from_bytes_into(buf: Bytes, packets: &mut Vec<...>)` slices each payload as a refcount view over the shared recv buffer. The recv loops in [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) and [`worker/reader.rs`](../../bluefin/src/worker/reader.rs) allocate one `BytesMut::with_capacity(15200)` per recv and `freeze()` it; all ~10 packets in the datagram share that one allocation. Cold paths (`deserialise`, `from_bytes` for handshake/tests) keep their `&[u8]` signature and pay one `Bytes::copy_from_slice` each. Bench impact was flat on its own (recv-side allocs weren't the binding constraint anymore) but landed the right shape for #4 below.
4. **Two-hop async path in `read_data`** ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs)) — packetize task → mpsc → sender task. Inline the `try_send` like `read_ack` already does. *Note: a prior attempt to unify with `tokio::select!` was unstable at 2.9 GB/s (see "Tried & rejected"). The second hop was deliberately introduced for +8.6% in `b8c0489`. Tread carefully.*
5. **Vectorized I/O is dead code**. [`bluefin-io/src/socket/udp_socket.rs`](../../bluefin-io/src/socket/udp_socket.rs) has `recvmsg_x`/`sendmsg_x` (macOS, behind `macos-fast` feature) and could have `recvmmsg`/`sendmmsg` (Linux), but `BluefinSocket` is **not wired into the runtime** — only used in tests. Highest single lever. Also: the existing `recvmsg_x` returns `Ok(1)` after receiving up to 8 messages — bug.
6. **Per-connection reader `connect()`s the socket** ([`net/connection.rs`](../../bluefin/src/net/connection.rs)), defeating `SO_REUSEPORT`. Multiple recv tasks on a connected socket race for the same datagram → no parallelism.
7. ~~**Wake-while-holding-lock**~~ — **DONE 2026-05 (#7).** `buffer_in_data_packets` and `buffer_in_ack_packets` in [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) now `take_waker_clone()` → `drop(guard)` → `wake()`. Flat on bench (already send-bound), correct shape.
8. ~~**`SlidingWindow::insert_packet_number` per-packet**~~ — **DONE 2026-05 (B).** [`utils/window.rs`](../../bluefin/src/utils/window.rs) gained `insert_range(base, count)` with a fast-path `extend` when the range is strictly past the back of the deque (the typical case for a 200-packet ack at steady state). [`net/ack_handler.rs::buffer_in_ack_packet`](../../bluefin/src/net/ack_handler.rs) calls it instead of looping `insert_packet_number`. Hygiene win — the bench is send-bound so no measurable Δ, but ack-receive cost drops from O(n) deque insertions to one bounds check + reserve + extend.
9. **`AckConsumer` is dead weight** — wakes on every ack batch to write an `AtomicU64` that nothing reads. Remove or wire to retransmission.
10. ~~**Unbounded send channel**~~ — **DONE 2026-05 (#10).** Writer's data channel is `mpsc::channel(4096)` (~6 MiB cap). Sync `BluefinConnection::send_bytes` returns `WriteError` on full; new async `send_bytes_async` awaits backpressure. Server peak unchanged within noise; delivered-bytes per run +~50% because the unbounded version was dropping ~half the bench's payloads at process exit. Ack channel left unbounded — low-volume, don't want a new failure mode. *Drain note (DONE 2026-05/D):* the bench client used to need a 2 s sleep after the loop because there was no public flush API. There is now: [`BluefinConnection::flush()`](../../bluefin/src/net/connection.rs) blocks until every byte handed to `send_bytes*` has been written to the kernel. The bench client uses it.
11. **Server handshake race (correctness, not throughput, but visible in benches)** — when two clients hello within ~100 ms of each other on macOS loopback, the second client's `connect()` times out at 3 s with `TimedOut("Failed to read from handshake connection buffer")`. Reproduces ~1 in 3 with 100 ms inter-client stagger; ~0 with 500 ms. Root cause is in [`bluefin/src/net/server.rs`](../../bluefin/src/net/server.rs) / [`bluefin/src/net/connection.rs`](../../bluefin/src/net/connection.rs): a hello arriving before the next `accept()` slot is wired up gets dropped. The accept-before-spawn fix in [`docs/archive/BINARY_RACE_CONDITIONS.md`](../../docs/archive/BINARY_RACE_CONDITIONS.md) closed the *processing* race but not the *buffering* race. Bench script papers over it with `--stagger 0.5` + `--retry`.

## Historical timeline (commits, what worked)

| Phase | Commit | Throughput | Key change |
|-------|--------|-----------|-----------|
| Baseline | `origin/main` | 2.50 GB/s | Original implementation |
| Foundation | `824c63f` | ~2.50 GB/s | Zero-copy `serialise_into` for header + packet (no direct gain; foundation for later wins) |
| Foundation | `2e2a39e` | ~2.50 GB/s | `MaybeUninit` recv buffers; removed `eprintln!` from hot paths |
| **Tier A** | `2e853c7` | 2.80 GB/s | **Buffer pool** of 12 datagrams; `recv_many` limit 10→20; `consume_data_into` writes into caller buffer (+12%) |
| Tier A | `1fe57b3` | 2.84 GB/s | `MAX_BUFFER_SIZE` 1000→2000, `MAX_SLIDING_WINDOW_SIZE` 20000→40000 (+1.4%) |
| **Tier A** | `b8c0489` | 3.04 GB/s | **Pipeline parallelism**: dedicated sender task in `read_data`, decouples packetize from `try_send` (+8.6%) |
| Tier 1 | — | — | `parking_lot::Mutex` attempted then reverted |
| Tier 2 | — | — | Unsafe `copy_nonoverlapping` in writer + `split_off` for chunking (replaced `drain`/`extend`) |
| Tier 3 | — | — | `VecDeque::with_capacity(64/128)`; lock scope tightened in `ReaderRxChannel::read`; `arrayvec`/`smallvec` deps added |
| **2026-05** (#1) | local | ~3.6–3.8 GB/s peak (no measurable Δ) | Recv-side: `mem::take(&mut packets)` instead of `clone`. Kept anyway: send-bound bench can't see the recv-side win. |
| **2026-05** (#2) | local | +5.6% client-side mb/s | Writer's data mpsc carries `Bytes` (not `Vec<u8>`); `BluefinConnection::send_bytes(Bytes)` gives callers a refcount-bump fast path. Bench client switched. |
| **2026-05** (#7) | local | flat (within noise) | `conn_reader.rs` now clones the `Waker` out of the buffer guard, drops the guard, *then* fires `wake()`. Stops the woken receiver task from immediately bouncing on `lock()`. Same fix on `ConnectionBuffer` and `AckBuffer`. Helper: `take_waker_clone()` on each. |
| **2026-05** (#10) | local | server flat; client-side +65–85% (was lying); delivered bytes +50% | Writer's data channel is now `mpsc::channel(4096)` (was `unbounded_channel`). New `send_bytes_async` on `BluefinConnection` awaits backpressure; sync `send_bytes` returns `WriteError` on full. Caps memory to ~6 MiB and surfaces a slow consumer instead of swallowing arbitrary backlog (the unbounded version was eating ~half the bench's payloads at process exit). Ack channel left unbounded — low-volume, don't want a new failure mode. |
| **2026-05** (B) | local | flat (send-bound bench can't see ack-side wins) | `SlidingWindow::insert_range(base, count)` with a fast-path `extend` when the new range is strictly past the back of the deque (~always at steady state). `AckBuffer::buffer_in_ack_packet` now calls it once per ack instead of looping `insert_packet_number` per packet — a 200-packet ack drops from 200 deque insertions to one bounds check + extend. Hygiene; correctness preserved by a new test that drives `insert_range` against a control deque populated by repeated `insert_packet_number`. |
| **2026-05** (D) | local | client-reported throughput now equals delivered throughput (was inflated); on "healthy" connection #1, server-delivered bytes = client-sent bytes (15,000,000,119 vs 15,000,000,119, byte-exact) | New `BluefinConnection::flush().await` truthfully blocks until every byte handed to `send_bytes*` has been written to the kernel via `socket.try_send`. Implementation: `Arc<AtomicUsize>` payload counter on `WriterHandler`; `fetch_add` on enqueue (with undo-on-error), `fetch_sub` inside the spawned sender task immediately after `socket.try_send` returns `Ok`; `tokio::sync::Notify` woken when the counter hits zero, with the standard double-check pattern in `flush()` to avoid the missed-wakeup race. Bench client's `sleep(Duration::from_secs(2))` replaced with `conn.flush().await?` — the wait is now exactly as long as the writer needs and never longer. **Side effect:** previously hidden conn-#0 server-side starvation (recv task can't keep up, kernel drops UDP) is now visible because `flush()` doesn't artificially extend the client's lifetime. That's a different bug; out of scope for this round. |
| **2026-05** (E) | local | flat on bench (peak ~3.83 GB/s either way) | `BluefinPacket.payload: Vec<u8>` → `Bytes`. Hot recv path (`from_bytes_into`) now takes an owned `Bytes` and slices each parsed payload as a refcount view over a single recv-buffer allocation — was up to ~10 `Vec::with_capacity(1500)` allocs per datagram, now one `BytesMut` per datagram (then refcount bumps). The recv loops in [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) and [`worker/reader.rs`](../../bluefin/src/worker/reader.rs) switched from stack `MaybeUninit<[u8; MAX]>` to heap `BytesMut::with_capacity(MAX)` + `set_len`/`truncate`/`freeze`. `BluefinPacketBuilder::payload` now takes `impl Into<Bytes>` so all existing call sites compile unchanged (`Vec<u8>` → `Bytes` is zero-copy via `Bytes::from`). `OrderedBytes::carry_over_bytes` switched from `Option<Vec<u8>>` to `Option<Bytes>` so split-off carry-over remains a refcount view. No measurable bench Delta on its own — recv-side allocs weren't the binding constraint — but architectural foundation for F. |
| **2026-05** (F) | local | peak 3.83 → 4.30 GB/s (**+12%**); fewer bilateral starvation events (2/5 runs both conns delivered full 15 GB, was 1/5) | Removed the conn_reader's mpsc+separate-buffer-task pipeline on macOS (where `get_number_of_tx_tasks() == 1`, so the tx → mpsc → rx shape was pure overhead). New `recv_and_buffer_inline` recvs, parses, and buffers in one task — saving one waker round-trip per recv (~250K/sec on the bench, ~25 ms/sec of latency-shaped overhead). Refactored `buffer_in_packets` and friends to take `&mut Vec<BluefinPacket>` and `drain(..)` so the carrier vec keeps its capacity across iterations (no `Vec<BluefinPacket>` alloc per recv). Linux multi-producer SO_REUSEPORT path is preserved unchanged. |

**Total documented gain**: 2.50 → ~4.30 GB/s peak (single conn, two-process bench) = **+72%** since baseline. Sustained per-conn `avg` ~1.85–1.95 GB/s.

## What's already implemented (don't re-propose)

Implementation details and rationale are in [`docs/archive/`](../../docs/archive/) for any deep dive.

### Allocation/copy elimination
- **Zero-copy header serialization**: `BluefinHeader::serialise_into(&mut [u8])` writes directly into a caller buffer, no intermediate `Vec`. ([`core/header.rs`](../../bluefin/src/core/header.rs))
- **Zero-copy packet serialization**: `BluefinPacket::serialise_into` plus an unsafe `serialize_packet_direct` fast path in the writer. ([`core/packet.rs`](../../bluefin/src/core/packet.rs), [`worker/writer.rs`](../../bluefin/src/worker/writer.rs))
- **Reusable parsed-packet `Vec`**: `BluefinPacket::from_bytes_into` parses into a caller-owned `Vec<BluefinPacket>` whose capacity is reused across iterations. (Inner per-packet payload `Vec`s are still allocated — see live bottleneck #3.)
- **Datagram buffer pool** (12 × `MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM`) in both `read_data` and `read_ack`. ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs))
- **`MaybeUninit` recv buffers** in both `ReaderTxChannel::run` and `ConnReaderHandler::tx_impl` — avoids zeroing 15200 B per recv.
- **`Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM)`** for outgoing assembly buffers (no growth reallocs).
- **`running_payload` pre-allocates `MAX_BLUEFIN_PAYLOAD_SIZE_BYTES * 2`** to absorb merges without a realloc.
- **`split_off`-based chunking + `mem::replace`/`mem::take`** in writer chunking and in `OrderedBytes` carry-over (replaced earlier `drain().collect()` patterns where measurement showed a win).
- **`ConnectionManager` keys are `(u32, u32)`** (was `String` via `format!`), avoiding ~1 alloc per packet routing decision.

### Concurrency / sync primitives
- **Lock-free `ConnectionManager` via `dashmap::DashMap`** — no global lock for routing.
- **`AtomicU64` for `largest_recv_acked_packet_num`** (was `RwLock<u64>`) — single MOV instead of async lock acquisition.
- **`Waker::will_wake` caching**: `set_waker_if_changed` skips the clone when the same task re-polls. ([`net/connection.rs`](../../bluefin/src/net/connection.rs), [`net/ack_handler.rs`](../../bluefin/src/net/ack_handler.rs))
- **Removed 5 µs `sleep` from `AckConsumer::run`** — future yields naturally; the sleep added pure latency.
- **Lock scope tightened in `ReaderRxChannel::read`** — only held during the `consume` call, not the post-processing.
- **`VecDeque::with_capacity(64)`** for `ack_queue`/`data_queue` in writer; **`with_capacity(128)`** for `ordered_packet_numbers` in `SlidingWindow`.

### Architecture
- **Pipeline parallelism in writer** ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs)): packetization task and socket-send task communicate via mpsc → CPU-bound packetization overlaps with I/O-bound sends. (Note: this is the source of the live "two-hop" bottleneck above; the win was real but the cost is now visible as the next ceiling.)
- **`recv_many` batching** with limit 20 in writer's data/ack consumers.
- **`MAX_BUFFER_SIZE = 2000`** for `OrderedBytes` (was 10M historically — that version blew the L2 cache and is what gave the 1000× memory reduction headline). 2000 entries × ~64 B = ~128 KB, fits comfortably in L2.
- **`MAX_SLIDING_WINDOW_SIZE = 40000`** for ack reception.
- **Insert-then-handshake split in `BluefinServer`/`BluefinClient`** — `pending_accept_ids` is `Vec<u32>` consumed FIFO via `remove(0)` (was `pop()`/LIFO, which mis-routed hellos).
- **Server accepts all connections then spawns processing tasks** (was inline spawn-per-accept, which mis-ordered handshakes).
- **Client adds 100 ms delay between connection spawns** so server-side `accept()` slots are ready before hello arrives.

### Socket / kernel
- **8 MB `SO_RCVBUF`/`SO_SNDBUF`** in [`utils::get_udp_socket_impl`](../../bluefin/src/utils/mod.rs) (was OS default ~64 KB).
- **512 KB `SO_RCVBUF`/`SO_SNDBUF`** in [`bluefin-io::BluefinSocket::new`](../../bluefin-io/src/socket/udp_socket.rs) (different code path; not yet on hot path).
- **`SO_NOSIGPIPE`** on macOS, **`UDP_SEGMENT 1500`** + `IP_PKTINFO` on Linux (in `bluefin-io`).
- **`SO_REUSEPORT`** + `SO_REUSEADDR` in `get_udp_socket_impl` (defeated by per-conn `connect()` — see live bottleneck #6).
- **`set_cloexec(true)`** to avoid leaking sockets across forks.
- **Best-effort CPU affinity tags on macOS** in [`utils/cpu_affinity.rs`](../../bluefin/src/utils/cpu_affinity.rs) (helper exists; not currently invoked anywhere).

### Build/lint
- `arrayvec = "0.7"` and `smallvec = "1.13"` added to `bluefin/Cargo.toml` for stack-allocated bounded collections — **not yet used in code**.
- `bytes = "1.9"` added at the workspace level — used in `bluefin-io` cmsghdr code, **not yet on the data path** (which is exactly bottleneck #2/#3).
- `#[inline]`/`#[inline(always)]` aggressively applied to hot serialization, header, and chunking functions.

## What NOT to retry — proven regressions

| Attempt | Result | Notes |
|---------|--------|-------|
| `mem::replace` for datagram clone (Round 1 attempt) | -5.5% | Reverted; later landed in different form (move into mpsc) |
| `split_off` in `OrderedBytes::consume` carry-over (replacing `drain().collect()`) | -2% | Compiler optimizes drain+collect well; trust it |
| `Vec::resize(n, 0)` then overwrite (vs `reserve` + `set_len` + write) | -50% (memset cost) | Use `reserve` + `set_len` + immediate write |
| Unsafe copy micro-opts on small slices | regressed | LLVM auto-vectorizes; manual usually loses |
| Writer batch size 32 (vs 12) | regressed to 2.46 GB/s | Larger batches hurt cache/latency |
| Unified writer with `tokio::select!` | unstable at 2.9 GB/s | Pipeline parallelism is the right shape |
| `parking_lot::Mutex` (Tier 1 attempt) | reverted | Worth re-trying on individual hot locks; broad swap regressed |
| Streaming buffer approaches | no measurable gain | |
| **2026-05 (G)**: Merge consumer's two lock acquisitions into one (peek + consume under same `poll`) | **peak 4.30 → 3.56 GB/s; bilateral-delivery rate 2/5 → 1/5** | The two-lock shape is *advantageous*: the producer (`recv_and_buffer_inline`) can grab the lock during the brief gap between the consumer's peek and consume. Collapsing them blocked the producer for the full peek+consume window. The hot-path comment in [`worker/reader.rs`](../../bluefin/src/worker/reader.rs) above `ReaderRxChannelFuture::poll` now records this; **don't re-collapse without measuring**. |

**Rules**:
- Trust the compiler. `drain().collect()` and `copy_from_slice` are well-optimized; don't replace them without a measurement.
- Architectural changes (pipeline parallelism, buffer pools, vectorized I/O) ≫ micro-optimizations.
- "Obviously faster" code regresses ~half the time. **Always measure.**
- A "slow" micro-pattern that the compiler recognizes (e.g. `copy_from_slice` → memcpy) is usually faster than a hand-rolled `unsafe` equivalent.

## Tactical rules of thumb

- **One alloc per datagram** is the budget on the recv side. Today we do ~10. Get to 1.
- **Zero allocs per `send` call** is the goal on the send side. Today we do 2 (`to_vec` + channel node).
- **Lock acquisitions per datagram** should be 1 (or 0 with lock-free). Today we do 2 on recv (buffer once, then read-side acquires again).
- **Wakeups per datagram**: today 2 on recv (mpsc + buffer waker), 3 on send (user→writer mpsc, writer→sender mpsc, syscall). Cut to 1 each side.
- Anything in `consume_data_into`/`consume_data` that touches `running_payload` more than once per byte is a bug-shaped optimization opportunity.
- A `Mutex::lock()` whose guard outlives any `wake_by_ref()` is a self-inflicted bounce — drop the guard first.

## Benchmarking protocol

The benchmark binaries were updated 2026-05-09 to add an idle timeout, instantaneous-throughput reporting, a `--task <ix>` per-process mode, and explicit error reporting on `connect()` / task-join failures. See [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs) and [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs).

**Use the script — don't roll your own:** [`bench_two_process.sh`](../../bench_two_process.sh) builds release, kills stale processes, spawns server + N client processes (one per task ix), waits for the server's idle-timeout exit, and prints a per-attempt summary. Each attempt's logs are preserved under `bench_logs/<timestamp>/attempt_<N>/`.

```bash
./bench_two_process.sh                  # 2 connections (default), 0.5 s stagger, retry up to 2x
./bench_two_process.sh -n 5             # 5 connections (max = DEFAULT_PORTS in client.rs)
./bench_two_process.sh --skip-build     # iterate without recompiling
./bench_two_process.sh --stagger 1.0    # widen client-spawn stagger if handshakes still race
./bench_two_process.sh --retry 5        # auto-retry up to 5x on the known handshake race
./bench_two_process.sh --help
```

The script auto-detects the documented handshake race (live bottleneck #11) by grepping client logs for `Failed to read from handshake connection buffer` and re-runs transparently. Without `--retry`, expect ~1-in-3 spurious failures at the default stagger.

**Measured baseline (2 processes, macOS loopback, 1500 B × 10M sends, 2026-05-09):**

| Mode | Per-conn `inst` peak | Per-conn `avg` | Per-conn client-side (queue may not fully drain) |
|------|---------------------|---------------|-------------------------------------------------|
| 1 client process, 2 tasks (legacy) | ~3.0 GB/s | ~3.0 GB/s | n/a |
| 2 client processes, 1 task each (script) | ~3.6 – 3.8 GB/s | ~1.8 GB/s | ~2.0 GB/s |

The `avg` drops in 2-process mode because both clients now compete for the loopback pair and each sees a longer cold-start window relative to the steady-state portion. The `inst` peak is *higher* because the writer pipeline isn't sharing a runtime with another sender. Trust `inst`/`peak` for ceiling; trust `avg` only for sustained-load comparisons.

Legacy single-process flow (still works for ad-hoc runs):

```bash
cargo clean && cargo build --release
pkill -9 server 2>/dev/null; pkill -9 client 2>/dev/null; sleep 1
./target/release/server &
sleep 2
./target/release/client --task 0 &
./target/release/client --task 1 &
wait
```

Single-process mode (`./target/release/client` without args) still works but is more vulnerable to the "client task #0 starves client task #1" effect described in the bottlenecks list.

The server prints two columns per connection:

```
0 avg <gb/s> | inst <gb/s> (read … kb/iter, min: … kb, max: … kb) (peak <gb/s>, trough <gb/s>)
```

`avg` is the cumulative running average since task start; `inst` is the throughput over the last ~3500 recvs. `peak`/`trough` track the *instantaneous* number, not the average. The FINAL line on exit prints total bytes, elapsed, average, and peak.

**Do not** trust a single run. Run 5×, drop high/low, average. Use the `inst` and `peak` numbers as the actual signal — `avg` is dragged down by the cold-start window.

**Variables to fix**:
- Same machine, same load (close browser, IDE indexing).
- Pin server and client to different cores if your OS supports it. macOS: see [`utils/cpu_affinity.rs`](../../bluefin/src/utils/cpu_affinity.rs) (best-effort affinity tags, not currently called).
- The client yields every 256 sends via `yield_now()`. Don't remove the yield — without it a tight `for` loop monopolises the worker and starves other tasks.
- The bench client calls `conn.flush().await?` after the send loop. **This is the right way to drain.** It blocks until every byte the producer handed to `send_bytes*` has been written to the kernel via `socket.try_send`. Don't replace it with `sleep(...)` — a fixed sleep was visibly too short on contended runs (server received ~40% of what the client claimed to send). The flush API also makes the client-reported throughput numbers honest: the wall-clock denominator now includes the actual on-the-wire-time, not a guess.
- Spawned-task errors are silent unless the binary explicitly handles them. `let _ = task.await` swallows both `JoinError` and the task's own `Result::Err`. As of 2026-05-09 the benchmark client `eprintln!`s `connect()` failures and exits non-zero — keep it that way or you'll be debugging by guesswork (this is exactly how the handshake race went unnoticed for so long).

## Profiling

- **macOS**: Instruments → Time Profiler / Allocations / System Trace. The Allocations instrument will immediately show the `payload.to_vec()` and `from_bytes_into` allocs.
- **Linux**: `perf record -g` → `perf report`; `flamegraph-rs` for SVG flamegraphs; `bpftrace` for syscall counts.
- **Tokio**: there's a commented `console_subscriber::init();` in both binaries. Uncomment to use `tokio-console` for task-level introspection (also requires the `tracing` feature on tokio, which is already on).

## Things the loopback benchmark hides

- **1500 B sends fit in exactly one packet/datagram** → `consume_data_into`'s merge path (used for >1500 B payloads) is never exercised. Production payloads of 3–5 KB will hit it hard.
- **`127.0.0.1` rarely pressures `SO_RCVBUF`/`SO_SNDBUF`** → recv batching wins (live bottleneck #5) won't show on loopback. Test over a real NIC, or `lo` with `tc qdisc add dev lo root netem rate 1gbit`.
- **2 connections** → `DashMap` contention is invisible. With many connections, `ReaderTxChannel` does an `Arc` clone of `ConnectionManagedBuffers` per datagram.
- **Single sender per process** (in `--task` mode) → can't observe scheduler starvation between connections.

For perf claims to be credible, you need a multi-connection (≥16) test, a >1500 B payload test, and a non-loopback (NIC, or at least `lo` with `tc netem`) test.

## When in doubt

- Forward-looking work: [`THROUGHPUT_ANALYSIS_2026.md`](../../THROUGHPUT_ANALYSIS_2026.md).
- Mental model: [bluefin-101](../bluefin-101/SKILL.md).
- Historical writeups (kept for archaeology): [`docs/archive/`](../../docs/archive/).

