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
5. **Vectorised I/O is blocked on application-level pacing**, not on missing bindings. The runtime path uses one syscall per datagram on both sides ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs) `socket.try_send`, [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) `socket.recv`). The macOS `sendmsg_x`/`recvmsg_x` bindings are written, tested, and live in [`bluefin/src/utils/macos_io.rs`](../../bluefin/src/utils/macos_io.rs) (the older [`bluefin-io/src/socket/udp_socket.rs`](../../bluefin-io/src/socket/udp_socket.rs) `BluefinSocket` is a separate dead path — do not extend it; use `macos_io` directly on the FD that tokio hands you). Three rounds (I = sendmsg_x writer alone, J = recvmsg_x reader alone, K = paired with `tokio::task::yield_now()` pacing) all regressed *delivered* throughput — see the rounds I/J/K rows in "Tried & rejected". Net guidance: **do not re-attempt vectorised I/O without first landing real pacing** (token bucket calibrated to drain rate, micro-sleep, or ack-window-based application flow control). The `macos_io` bindings stay in tree precisely so a future round can drop in pacing without re-doing the syscall plumbing.
6. **Per-connection reader `connect()`s the socket** ([`net/connection.rs`](../../bluefin/src/net/connection.rs)), defeating `SO_REUSEPORT`. Multiple recv tasks on a connected socket race for the same datagram → no parallelism.
7. ~~**Wake-while-holding-lock**~~ — **DONE 2026-05 (#7).** `buffer_in_data_packets` and `buffer_in_ack_packets` in [`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs) now `take_waker_clone()` → `drop(guard)` → `wake()`. Flat on bench (already send-bound), correct shape.
8. ~~**`SlidingWindow::insert_packet_number` per-packet**~~ — **DONE 2026-05 (B).** [`utils/window.rs`](../../bluefin/src/utils/window.rs) gained `insert_range(base, count)` with a fast-path `extend` when the range is strictly past the back of the deque (the typical case for a 200-packet ack at steady state). [`net/ack_handler.rs::buffer_in_ack_packet`](../../bluefin/src/net/ack_handler.rs) calls it instead of looping `insert_packet_number`. Hygiene win — the bench is send-bound so no measurable Δ, but ack-receive cost drops from O(n) deque insertions to one bounds check + reserve + extend.
9. **`AckConsumer` is reserved for flow control** — currently writes `largest_recv_acked_packet_num` to an `AtomicU64` that nothing reads, but **do NOT delete it**. It is the wiring point for the application-level pacing / ack-window flow control that vectorised I/O (live bottleneck #5, rounds I/J/K) needs. Removing it now would force re-implementing the same plumbing later. The waker hop + per-ack atomic store is cheap (~one extra task wake per 200-packet ack). Wire it to the writer's pacing logic when implementing flow control.
10. ~~**Unbounded send channel**~~ — **DONE 2026-05 (#10).** Writer's data channel is `mpsc::channel(4096)` (~6 MiB cap). Sync `BluefinConnection::send_bytes` returns `WriteError` on full; new async `send_bytes_async` awaits backpressure. Server peak unchanged within noise; delivered-bytes per run +~50% because the unbounded version was dropping ~half the bench's payloads at process exit. Ack channel left unbounded — low-volume, don't want a new failure mode. *Drain note (DONE 2026-05/D):* the bench client used to need a 2 s sleep after the loop because there was no public flush API. There is now: [`BluefinConnection::flush()`](../../bluefin/src/net/connection.rs) blocks until every byte handed to `send_bytes*` has been written to the kernel. The bench client uses it.
11. **Server handshake race (correctness, not throughput, but visible in benches)** — when two clients hello within ~100 ms of each other on macOS loopback, the second client's `connect()` times out at 3 s with `TimedOut("Failed to read from handshake connection buffer")`. Reproduces ~1 in 3 with 100 ms inter-client stagger; ~0 with 500 ms. Root cause is in [`bluefin/src/net/server.rs`](../../bluefin/src/net/server.rs) / [`bluefin/src/net/connection.rs`](../../bluefin/src/net/connection.rs): a hello arriving before the next `accept()` slot is wired up gets dropped. The accept-before-spawn fix in [`docs/archive/BINARY_RACE_CONDITIONS.md`](../../docs/archive/BINARY_RACE_CONDITIONS.md) closed the *processing* race but not the *buffering* race. Bench script papers over it with `--stagger 0.5` + `--retry`.
12. **Bench-only: per-recv `tokio::time::timeout(2s, conn.recv(...))` is the single largest server hotspot at ~5 % CPU.** Surfaced by the 2026-05-10 server flamegraph (`server::run::_{{closure}}` self-time 6.19 %, dominated by `Timespec::now` → `clock_gettime` → `mach_absolute_time` chains totalling ~485 samples). Every recv arms and disarms a fresh `tokio::time::Sleep` on the timer wheel; at ~270K recvs/s on the bench that's >540K `clock_gettime` calls/s. Fix is bench-only in [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs): track `Instant::now()` on a coarse cadence (e.g. every 1024 iterations) and exit the loop if `last_recv.elapsed() > RECV_IDLE_TIMEOUT`. **Library is unaffected.** Important because freeing this CPU exposes the true library ceiling under measurement — current ~4.48 GB/s peak almost certainly under-reports. (No round number assigned; bench-side change.)
13. **`OrderedBytes::consume` memcpy on the recv path is ~5 % library CPU** (`_platform_memmove` 4.98 % in the flamegraph, parented by `server::run` → `conn.recv`). The recv pipeline parses payloads as zero-copy `Bytes` slices over the shared 15 KiB recv buffer, but `consume()` pays one `copy_from_slice(payload)` into the user-supplied `&mut [u8]`. Fix: add a parallel `BluefinConnection::recv_bytes()` API returning the underlying `Bytes` slice (true kernel-→-user zero-copy). Keep existing `recv(&mut [u8])` semantics for callers with stack buffers — don't break the API. Bench client/server switch to `recv_bytes`. Touches [`bluefin/src/net/connection.rs`](../../bluefin/src/net/connection.rs), [`bluefin/src/net/ordered_bytes.rs`](../../bluefin/src/net/ordered_bytes.rs), [`bluefin/src/worker/reader.rs`](../../bluefin/src/worker/reader.rs). Most likely-binding remaining library bottleneck on the recv side. (No round number yet.)
14. **Per-recv `BytesMut::with_capacity(15200)` in `recv_and_buffer_inline` is ~1.2 % in malloc** (`szone_malloc_should_clear` 0.48 % + `small_malloc_should_clear` 0.40 % + `small_malloc_from_free_list` 0.27 %, all parented by `ConnReaderHandler::start::_{{closure}}`). Same shape as the round-N (writer-side) fix but trickier on the recv side because the derived `Bytes` is held by `OrderedBytes` until the consumer drains it — the recv loop can't recycle a buffer until every refcount on the slices it shipped has dropped. A small SPMC-of-`BytesMut` pool with `Bytes::try_into_mut()` recycle on `consume()` works but is non-trivial. ~1-2 % upside, low risk if done after #13 (which makes the consumer side honest). (No round number yet.)
15. **Round-N recycle channel is unbounded** — at process exit it holds ~hundreds of empty 15 KiB `Vec<u8>`s waiting to be dropped, and tokio's task shutdown frees them serially through `madvise(DONT_NEED)` per Vec. Surfaced by the 2026-05-10 client flamegraph as **21.9 % of all leaf samples** rooted at `drop_in_place<UnboundedReceiver<Vec<u8>>>` → `drop_in_place<WriterHandler::read_data::{{closure}}>` → `task::raw::shutdown`. Doesn't slow steady-state throughput (it's strictly post-flush) but bloats profile noise and makes graceful shutdown surprisingly slow. Trivial fix: `mpsc::channel::<Vec<u8>>(16)` instead of `unbounded_channel`; sender's `recycle_tx.send(datagram).ok()` becomes `recycle_tx.try_send(datagram).ok()` so when the channel is full the empty Vec drops in place. At steady state cycle is 1:1 so the bound never fires; at exit there's ≤16 Vecs to drop. **Pure hygiene**; no throughput impact expected. Touches [`bluefin/src/worker/writer.rs`](../../bluefin/src/worker/writer.rs).
16. **Bench-only client-side `clock_gettime` storm in `client::run_connection`** — same family as #12 but on the client side. The 2026-05-10 client flamegraph shows `client::run_connection::_{{closure}}` self-time at **5.53 %** with the usual `Timespec::now`/`clock_gettime`/`mach_absolute_time` chain underneath. Bench loop in [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs) calls `Instant::now()` per-iteration for throughput accounting. Coarsen to every-N like the server-side fix. **Library is unaffected.**

## Last profiling pass

### 2026-05-10 — server flamegraph

`flamegraph.svg` at repo root (later overwritten by the client capture below — re-run if you need the raw SVG). ~9 180 samples / ~9 s of one healthy bench connection.

Top hotspots, attributed to call sites:

| Self %  | Symbol                                              | Caller / category                                            | Verdict                                                    |
|--------:|-----------------------------------------------------|--------------------------------------------------------------|------------------------------------------------------------|
| 6.19 %  | `server::run::_{{closure}}::_{{closure}}`           | bench loop body (timeouts + accounting)                      | **bench-only** — see live bottleneck #12                   |
| 5.95 %  | `parking_lot::Condvar::wait_until_internal`         | tokio worker park (idle)                                     | normal idle parking, not waste                             |
| 5.79 %  | `__psynch_cvwait`                                   | ↑ same                                                       | kernel side of idle parking                                |
| 4.98 %  | `_platform_memmove`                                 | `OrderedBytes::consume` → user buf                           | **library** — see live bottleneck #13                      |
| 4.49 %  | `ConnReaderHandler::start::_{{closure}}` (self)     | recv loop body                                               | normal recv work                                           |
| 3.73 %  | `__sendto`                                          | writer (acks)                                                | intrinsic — already batched in `read_ack`                  |
| 4.85 %  | `Timespec::now` + `clock_gettime` + `mach_absolute_time` | `server::run::_{{closure}}` (per-recv `timeout` Sleep) | **bench-only** — same root as #12                          |
| ~5.8 %  | `Condvar::notify_one_slow` + `__psynch_cvsignal` (×2 paths) | cross-thread task wakes                              | wake density; reducing means batching more per wake (latency cost) |
| 1.25 %  | `bytes::shared_drop`                                | `server::run` recv (consumer side)                           | refcount drop; goes away with #13's zero-copy path         |
| ~1.15 % | `szone_malloc` + `small_malloc_*`                   | `ConnReaderHandler` recv loop (`BytesMut::with_capacity`)    | **library** — see live bottleneck #14                      |
| 0.79 %  | `OrderedBytes::buffer_in_packet`                    | `ConnReaderHandler` recv path                                | normal buffer work                                         |

**Calibration after #12.** Once the bench-side `clock_gettime` storm is gone, expect roughly +5–6 % apparent server throughput at the same library cost, because the saved CPU goes to actually draining the recv path. Re-profile after the fix; #13 should then climb the chart proportionally.

### 2026-05-10 — client flamegraph

~15 000 samples / ~9 s of one healthy bench connection. **The headline is striking: roughly half the profile is real work, half is idle/teardown.**

| Bucket | Leaf samples | % |
|---|---:|---:|
| Steady-state work | 1 057 | **48.3 %** |
| Worker idle/park | 652 | 29.8 % |
| Process-exit teardown | 478 | **21.9 %** |

**The teardown 22 % is entirely the round-N recycle channel** — every leaf in that bucket roots at `drop_in_place<UnboundedReceiver<Vec<u8>>>` → `drop_in_place<WriterHandler::read_data::{{closure}}>` → `task::core::set_stage` → `task::raw::shutdown`, with `__recvfrom`/`madvise`/`free_small`/`mvm_deallocate_plat` as leaves. The unbounded recycle channel hoards empty 15 KiB Vecs at exit and tokio's task drop frees them serially. See live bottleneck #15.

**Real steady-state hotspots** (filtering out shutdown + idle):

| Self %  | Symbol                                                       | Caller                              | Verdict                                                    |
|--------:|--------------------------------------------------------------|-------------------------------------|------------------------------------------------------------|
| 5.53 %  | `client::run_connection::_{{closure}}` self                  | bench send loop                     | **bench-only** — `clock_gettime` storm; see #16            |
| 1.71 %  | `tokio::sync::mpsc::list::Rx<T>::pop`                        | writer's `recv_many`                | mpsc internals; intrinsic                                  |
| 1.15 %  | `szone_malloc_should_clear`                                  | mpsc block alloc + `Bytes` clone in bench | bench-allocation noise                              |
| 0.74 %  | `tokio::sync::mpsc::list::Tx::find_block`                    | `client::run_connection`            | client mpsc block-alloc on send → backpressure path        |
| 0.76 %  | `__recvfrom`                                                 | client recv (acks)                  | intrinsic syscall                                          |
| 0.55 %  | `_szone_free`                                                | various                             | dropping freed `Bytes`/`Vec`s                              |

**Things that are NOT bottlenecks on the client.** The library send hot path doesn't break threshold at all: `consume_data_into`, `serialize_packet_direct`, `__sendto` from the writer's spawned sender — none above 0.5 %. **Send-side library is essentially out of obvious low-hanging fruit on the client.** The biggest finding (#15) is mostly a profiler artifact (post-shutdown, doesn't affect throughput) but worth fixing for clean profiles + graceful shutdown.

### Reproducing / re-grabbing

Both SVGs were produced via `cargo flamegraph --bin <server|client> -- ...` (cargo-flamegraph + dtrace under sudo on macOS). The SVG is `inferno-flamegraph` format with raw integer `x`/`w` stored in a custom `{http://github.com/jonhoo/inferno}` namespace — **the public `x`/`width` attrs are percent strings, not floats**, so any parser must read the inferno-namespace attrs. A working extractor lives at `/tmp/flame.py` from the 2026-05-10 session; key parser detail:

```python
INFERNO = '{http://github.com/jonhoo/inferno}'
x = float(rect.get(INFERNO + 'x'))   # NOT rect.get('x') — that's a percent
w = float(rect.get(INFERNO + 'w'))
```

The teardown-vs-steady-vs-idle bucketing is also worth replicating when reading any future client flamegraph: walk each leaf box's parent chain; if any ancestor symbol contains `task::raw::shutdown`, `set_stage`, or `drop_in_place`, that leaf is teardown. If any ancestor contains `park_internal`, `wait_until`, or `cvwait`, it's idle. Otherwise it's real work. On a graceful-exit profile, expect 20-25 % teardown unless #15 is fixed.

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
| **2026-05-10** (L) | local | bilateral 7/10 → 8/10 (+1 healthy run); throughput flat (1.87 → 1.82 GB/s avg, 4.00 → 4.04 GB/s peak — within noise) | Bumped requested `SO_RCVBUF`/`SO_SNDBUF` from 8 MB → 32 MB in [`bluefin/src/utils/mod.rs`](../../bluefin/src/utils/mod.rs)'s `get_udp_socket_impl`. **Requires** `sudo sysctl -w kern.ipc.maxsockbuf=33554432 net.inet.udp.recvspace=8388608` on macOS at runtime, otherwise the kernel silently caps the actual buffer at the existing `kern.ipc.maxsockbuf` (8 MB by default). Kept as a small reliability win for the steady-state path. Discovered while investigating round K — the buffer is *not* the binding constraint for paired vectorised I/O on macOS, but it does reduce transient overrun in the per-datagram baseline. |
| **2026-05-10** (M) | local | flat (median avg 1.76 GB/s, peak 4.09 GB/s, bilateral 15/20 over 10 runs — within round-L noise) | Eliminated the `payload_bytes_in_datagram` per-send walk in [`bluefin/src/worker/writer.rs`](../../bluefin/src/worker/writer.rs). Was: spawned sender task scanned every 20-byte header in the just-formatted ~15 KiB datagram to recover the user-payload count for `pending_bytes`/`flush()` accounting. Now: `consume_data_into` returns `usize` (payload bytes packed; `0` = nothing produced) — the count is computed for free as it already iterates `max_bytes_to_take` per packet. Channel changed from `mpsc::unbounded_channel::<Vec<u8>>` to `mpsc::unbounded_channel::<(Vec<u8>, usize)>` so the sender consumes the count without re-walking. `payload_bytes_in_datagram` kept as `#[allow(dead_code)]` — it's still a useful audit/debug helper, just not on the hot path. Hygiene win: ~750 byte-reads + 750 cmp+adds per datagram removed; bench is bound elsewhere (probably writer→sender mpsc hop + per-conn `connect()`-on-recv-socket from live bottleneck #6) so no measurable Δ surfaces. |
| **2026-05-10** (N) | local | peak 4.09 → 4.48 GB/s (**+10%**); median peak 3.58 → 3.84 GB/s (**+7%**); median avg 1.76 → 1.84 GB/s (**+5%**); bilateral 15/20 → 31/40 (~flat ~78%); 0 hangs over 20 runs | Closed the "the buffer pool is a lie" gap in [`bluefin/src/worker/writer.rs`](../../bluefin/src/worker/writer.rs)::`read_data`. Was: 12-Vec `datagram_pool` allocated once at startup, but every `mem::replace(&mut datagram_pool[i], Vec::with_capacity(15200))` swapped in a *fresh* allocation and the just-sent vec was dropped by the spawned sender after `try_send` returned `Ok` — i.e. one 15 KiB heap allocation + free per outgoing datagram, regardless of the pool. Now: a second unbounded mpsc channel ships emptied vecs sender → packetiser. Sender's hot loop ends with `datagram.clear(); let _ = recycle_tx.send(datagram);` (clear keeps capacity — `len = 0` write, no realloc). Packetiser's swap line becomes `recycle_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(15200))` — at steady state the recycle channel always has a Vec ready and the fallback never triggers. Initial pool of 12 + the writer's `mpsc::channel(4096)` cap bound the in-flight vec count; unbounded recycle channel is fine because cycle is 1:1 at steady state. The `read_ack` task uses a similar but already-honest pool (single-task, no cross-task hop, just `pending_send` swap on backpressure) and was left unchanged. The mpsc allocator pressure was apparently a real bottleneck — clean +10% peak with no other shape change. |

**Total documented gain**: 2.50 → ~4.48 GB/s peak (single conn, two-process bench) = **+79%** since baseline. Sustained per-conn `avg` ~1.84 GB/s (median over 20 paired runs).

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

### macOS batched-I/O bindings (compiled, dead in runtime)
- [`bluefin/src/utils/macos_io.rs`](../../bluefin/src/utils/macos_io.rs) provides `sendmsg_x_connected(fd, &[&[u8]])` and `recvmsg_x_into(fd, &mut [&mut [u8]], &mut [usize])` over Apple's undocumented-but-stable `sendmsg_x`/`recvmsg_x` syscalls (xnu `bsd/sys/socket_private.h`, `MAX_BATCH=16`, stack-local iovec/MsghdrX arrays, `EAGAIN` → `ErrorKind::WouldBlock` for `tokio::net::UdpSocket::async_io(Interest, closure)`). **Compiled, tested via the failed rounds I/J/K, currently has no runtime caller.** Re-wire from the writer's spawned sender task and from `recv_and_buffer_inline` once a pacing layer is in place — see live bottleneck #5.

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
| **2026-05-10 (H)**: Swap `data_queue: VecDeque<Bytes>` for the in-tree `RingBuffer<Bytes>` (cap 256) in `WriterHandler::read_data` | **bilateral 6/10 → 2/10; per-conn 75% → 55%; healthy avg 1.81 → 1.77 GB/s; healthy peak 3.78 → 3.60 GB/s; max peak 4.60 → 4.32 GB/s** (same-session A/B on 10 runs each, [`bench_two_process.sh`](../../bench_two_process.sh) `--retry 5`) | `RingBuffer` in [`bluefin/src/utils/ring_buffer.rs`](../../bluefin/src/utils/ring_buffer.rs) is strictly worse than `VecDeque::with_capacity(N)` for the writer's hot queue. Three reasons: (1) `RingBuffer::new(cap)` does `vec![Bytes::default(); cap]` — initializes every slot up-front (8 KB write at startup) where `VecDeque` reserves uninit; (2) `RingBuffer::pop_front` uses `mem::take` (default-write into the slot) where `VecDeque::pop_front` is `ptr::read` (no slot rewrite, no Drop); (3) `RingBuffer::push_back` is `self.buffer[tail] = value` (bounds check + Drop on previous occupant + move) where `VecDeque::push_back` is `ptr::write` (no bounds check, no Drop). Net per-op cost is small but compounds across millions of packets. The `RingBuffer` scaffold is kept for cases where its strict bounded semantics are a feature, not a cost — but **do not use it on the writer hot path**. If you want a strict-bounded fast queue, the win has to come from `MaybeUninit` slots + raw `ptr::read`/`ptr::write` + power-of-two mask, not the current implementation. |
| **2026-05-10 (I)**: macOS `sendmsg_x` batch send (Apple's `sendmmsg` equivalent) on the writer hot path; `socket.try_send` per-datagram → `async_io(WRITABLE, sendmsg_x_connected(fd, &views[..n]))` batching up to 16 datagrams per syscall | **syscall-level peak 4.5 → 6.0 GB/s** (genuine win!) **but server-side delivered throughput collapses: bilateral 6/10 → 0/10; server-received bytes ~3.7 GB out of 15 GB sent per conn (75% loss)** | The syscall is faster but bursts overrun the server's recv buffer. macOS `kern.ipc.maxsockbuf = 8 MB` is the absolute cap (we already request that via `set_recv_buffer_size(8MB)` in [`bluefin/src/utils/mod.rs`](../../bluefin/src/utils/mod.rs)). At 5 GB/s drain, 8 MB = **1.6 ms of buffering** — any pause longer than that drops UDP datagrams silently. The client's `sendmsg_x` reports success (kernel accepted bytes) so the client is *unaware* of drops; only server-side FINAL reveals the loss. The `macos_io::sendmsg_x_connected` binding in [`bluefin/src/utils/macos_io.rs`](../../bluefin/src/utils/macos_io.rs) is preserved for a future round that pairs it with `recvmsg_x` on the reader side. **Lesson**: vectorised I/O **must be paired** — faster sends without faster receives just exposes drops. Wire `recvmsg_x` first, then re-introduce `sendmsg_x`. Also: `tokio::net::UdpSocket::async_io(Interest::WRITABLE, closure)` is the correct primitive for bypassing tokio's per-datagram syscall path; manual `try_io` + `writable().await` livelocks. |
| **2026-05-10 (J)**: macOS `recvmsg_x` batch recv on `recv_and_buffer_inline` reader hot path; per-recv `socket.recv(&mut buf[..]).await` → `async_io(READABLE, recvmsg_x_into(fd, &mut bufs[..16], &mut lens[..16]))` draining up to 16 datagrams per syscall. Slot-pool of `[Option<BytesMut>; 16]` re-allocated per consumed slot. | **bilateral 7/10 → 5/10; healthy avg 1.87 → 1.71 GB/s (-9 %); healthy peak 4.00 → 3.15 GB/s (-21 %); 0 hangs** (10-run A/B vs the post-F baseline at 8 MB recv buffer) | At the writer's current pace, the kernel's UDP recv queue almost never holds >1 datagram when readiness fires, so each `recvmsg_x` returns 1 datagram and pays the full per-call setup cost (16-iovec stack array + 16 sockaddr scratch + the `MaybeUninit<&mut [u8]>` array transmute trick + the `async_io` closure overhead). That setup is *more expensive* than vanilla `recv()`. **Lesson**: a vectorised recv only wins when the writer also bursts. The reader-only spike is the wrong shape for unilateral introduction. The `macos_io::recvmsg_x_into` binding in [`bluefin/src/utils/macos_io.rs`](../../bluefin/src/utils/macos_io.rs) is preserved. |
| **2026-05-10 (K)**: paired macOS `sendmsg_x` writer (`PACE_BATCH=4` or `8` with `tokio::task::yield_now()` after each burst) + `recvmsg_x` reader (`MAX_BATCH=16`, intentionally larger than the writer's batch so the reader can always drain in one syscall). Two error-handling variants tested: "exit task on send error" (v1) and "drain accounting + continue" (v2). | **Headline: faster healthy-run throughput, worse delivery reliability.** Best healthy throughput: peak 4.84 GB/s (BATCH=8) and 4.35 GB/s (BATCH=4) vs baseline 4.00. Best reliability: BATCH=4 v1 → bilateral 8/10 (vs 7/10 baseline) **but with 1 hang in 10 runs**. BATCH=4 v2 (no-exit) → 0 hangs but bilateral collapses to 4/10. Bumping `kern.ipc.maxsockbuf` from 8 MB → 32 MB and the socket request from 8 MB → 32 MB does **not** help paired (5/10 bilateral, throughput flat) — the recv-buffer cap was not the binding constraint. | Three independent failure modes interact: (a) `tokio::task::yield_now()` is a no-op when no other task is queued on the same worker, so the writer still outpaces the reader's drain rate; (b) macOS `sendmsg_x` faithfully surfaces `ECONNREFUSED` mid-stream if the peer's socket dies (handshake-race fallout), where `try_send` swallowed it — exit-on-error then hangs `flush()` because `pending_bytes` accumulates with no consumer; drain-and-continue avoids the hang but lets the application generate undeliverable data; (c) most loss is 0.7–3 % per failed run (recv-buffer transients), which is the classic "writer slightly faster than the reader can drain" pattern that real flow control would fix. **Lesson**: `yield_now` is too weak as pacing on macOS. To unlock vectorised I/O, you need **either** (i) explicit micro-sleep / token bucket calibrated to the measured drain rate, or (ii) ack-window-based application flow control (Bluefin doesn't have one wired into the writer's sendmsg path). The `macos_io` module remains in tree for that future attempt. **Side win**: the buffer bump from 8 MB → 32 MB (with `sudo sysctl -w kern.ipc.maxsockbuf=33554432`) gave the *baseline* path bilateral 7/10 → 8/10 with throughput unchanged — kept as a standalone change. |

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
- **`tokio::task::yield_now()` is not pacing on macOS.** When the worker has no other ready task it returns immediately, so a writer that yields after every burst still saturates the kernel UDP queue inside one scheduling tick. Use `tokio::time::sleep(Duration::from_micros(N))`, a token bucket, or ack-window gating instead. (Round K, 2026-05-10.)
- **macOS UDP recv buffer is hard-capped at `kern.ipc.maxsockbuf`** (default 8 MB; bump with `sudo sysctl -w kern.ipc.maxsockbuf=33554432 net.inet.udp.recvspace=8388608`). At 5 GB/s drain, 8 MB is **1.6 ms of buffering** — any pause longer than that drops UDP datagrams silently. The socket-creation path in [`bluefin/src/utils/mod.rs`](../../bluefin/src/utils/mod.rs) requests 32 MB; it only takes effect after the sysctl bump.
- **`socket.try_send` swallows `ECONNREFUSED`; `sendmsg_x` does not.** Any future vectorised-send experiment must either propagate the error to a `BluefinError::ConnectionLost` *or* drain `pending_bytes` and continue — returning from the spawned sender task hangs `flush()` because new enqueues accumulate without a consumer. (Round K, 2026-05-10.)
- **Vectorised I/O must be paired**. A reader-side batch (`recvmsg_x`) wins only when the writer also bursts; otherwise each call returns 1 datagram and pays the per-call setup overhead (round J: −9 % avg, −21 % peak). A writer-side batch (`sendmsg_x`) without a faster reader just exposes recv-buffer drops faster (round I: 75 % loss). Land them together — with pacing.

## Benchmarking protocol

The benchmark binaries were updated 2026-05-09 to add an idle timeout, instantaneous-throughput reporting, a `--task <ix>` per-process mode, and explicit error reporting on `connect()` / task-join failures. See [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs) and [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs).

**Pick by intent — there are two scripts:**

| Goal | Script | Why |
|---|---|---|
| One-off ad-hoc throughput measurement, N connections, retry-on-handshake-race | [`bench_two_process.sh`](../../bench_two_process.sh) | Has `-n`, `--stagger`, `--retry`, `--skip-build`; preserves `bench_logs/<ts>/attempt_<N>/` |
| Statistical sweep (10-run loop), or any context where a hung run must not block the whole job | [`bench_run_with_timeout.sh`](../../bench_run_with_timeout.sh) | Self-contained wall-clock watchdog, no `gtimeout` needed, prints `WALLCLOCK_TIMEOUT` on hang |

`bench_two_process.sh` only enforces a per-client timeout when `gtimeout` (GNU coreutils) or `timeout` is on PATH. **On a stock macOS install neither is**, so a hung client will block until you ^C — exactly the gap `bench_run_with_timeout.sh` plugs. Use it for any automation.

**`bench_two_process.sh` (interactive):**

```bash
./bench_two_process.sh                  # 2 connections (default), 0.5 s stagger, retry up to 2x
./bench_two_process.sh -n 5             # 5 connections (max = DEFAULT_PORTS in client.rs)
./bench_two_process.sh --skip-build     # iterate without recompiling
./bench_two_process.sh --stagger 1.0    # widen client-spawn stagger if handshakes still race
./bench_two_process.sh --retry 5        # auto-retry up to 5x on the known handshake race
./bench_two_process.sh --help
```

**`bench_run_with_timeout.sh` (single shot, hard cap; assumes you've already `cargo build --release`):**

```bash
./bench_run_with_timeout.sh             # 30 s wall-clock cap (default)
./bench_run_with_timeout.sh 25          # 25 s cap; what we used for the round-J/K sweeps
# Logs land in bench_logs/<timestamp>_to/{server,c0,c1}.log
```

**The 10-run sweep pattern (round-J/K dataset, 2026-05-10):**

```bash
cargo build --release
rm -f /tmp/bench_results.txt
for i in $(seq 1 10); do
    echo "=== run $i ===" | tee -a /tmp/bench_results.txt
    for try in 1 2 3 4 5; do
        OUT=$(./bench_run_with_timeout.sh 25 2>&1)
        echo "$OUT" >> /tmp/bench_results.txt
        # Retry only on the known handshake race; surface hangs and stop.
        if echo "$OUT" | grep -qE "Failed to read from handshake"; then continue; fi
        if echo "$OUT" | grep -q "WALLCLOCK_TIMEOUT"; then break; fi
        break
    done
done
echo "hangs:           $(grep -c WALLCLOCK_TIMEOUT /tmp/bench_results.txt)"
echo "full deliveries: $(grep -c 'FINAL: 15000000119' /tmp/bench_results.txt)"
```

The script auto-detects the documented handshake race (live bottleneck #11) by grepping client logs for `Failed to read from handshake connection buffer`; the parent loop above re-runs transparently. Without `--retry`, expect ~1-in-3 spurious failures at the default stagger.

**Measured baseline (2 processes, macOS loopback, 1500 B × 10M sends, 2026-05-09):**

| Mode | Per-conn `inst` peak | Per-conn `avg` | Per-conn client-side (queue may not fully drain) |
|------|---------------------|---------------|-------------------------------------------------|
| 1 client process, 2 tasks (legacy) | ~3.0 GB/s | ~3.0 GB/s | n/a |
| 2 client processes, 1 task each (script) | ~3.6 – 3.8 GB/s | ~1.8 GB/s | ~2.0 GB/s |

The `avg` drops in 2-process mode because both clients now compete for the loopback pair and each sees a longer cold-start window relative to the steady-state portion. The `inst` peak is *higher* because the writer pipeline isn't sharing a runtime with another sender. Trust `inst`/`peak` for ceiling; trust `avg` only for sustained-load comparisons.

### 10-run reference dataset (post-rounds B + D + E + F, 2026-05-10)

Captured immediately after rounds E (`payload: Bytes`) and F (inline recv→buffer) landed; serves as the steady-state reference point for all future tuning. Two-process bench, 1500 B × 10M sends per client, default stagger.

Per-run (server-side delivered bytes; ✗ = starved):

| run | c0 deliv (GB) | c0 avg | c0 peak | c0 ✓ | c1 deliv (GB) | c1 avg | c1 peak | c1 ✓ | both? |
|----:|--------------:|-------:|--------:|:----:|--------------:|-------:|--------:|:----:|:-----:|
|  1  | 15.00 | 1.93 | **4.44** | ✓ | 15.00 | 1.81 | 4.36 | ✓ | ✓ |
|  2  | 15.00 | 1.93 | 3.61 | ✓ | 15.00 | 1.79 | 3.59 | ✓ | ✓ |
|  3  |  0.05 | 0.02 | 0.07 | ✗ | 15.00 | 1.82 | 4.03 | ✓ | ✗ |
|  4  | 15.00 | 1.95 | **4.51** | ✓ | 15.00 | 1.81 | 4.14 | ✓ | ✓ |
|  5  | 15.00 | 1.95 | 3.80 | ✓ | 15.00 | 1.80 | 4.11 | ✓ | ✓ |
|  6  | 15.00 | 1.97 | 4.30 | ✓ | 15.00 | 1.84 | 3.89 | ✓ | ✓ |
|  7  | 15.00 | 1.97 | 3.85 | ✓ |  7.74 | 1.36 | 4.03 | ✗ | ✗ |
|  8  | 11.09 | 1.75 | 3.54 | ✗ | 15.00 | 1.79 | 3.56 | ✓ | ✗ |
|  9  | 15.00 | 1.90 | 3.69 | ✓ | 15.00 | 1.79 | 3.75 | ✓ | ✓ |
| 10  | 15.00 | 1.95 | 3.63 | ✓ | 15.00 | 1.81 | 4.19 | ✓ | ✓ |

**Reliability (20 connection-runs):**
- Bilateral full delivery (both conns 15 GB): **7/10** runs (was 2/5 in earlier rounds, +75%).
- Per-conn full delivery rate: 17/20 (85%).
- Total bytes delivered: 273.89 / 300 GB = **91.3%**.

**Aggregates (all 20 conn-runs):**

| Metric | Mean | Median | Stdev | Min | Max |
|---|---:|---:|---:|---:|---:|
| Server avg GB/s | 1.75 | 1.81 | 0.43 | 0.02 | 1.97 |
| Server peak GB/s | 3.75 | 3.87 | 0.92 | 0.07 | **4.51** |
| Client elapsed (s) | 5.24 | 5.22 | 0.11 | 5.01 | 5.40 |

**Healthy-only (7 runs where both conns delivered 15 GB) — the steady-state number:**

| Metric | Mean | Median | Stdev | Min | Max |
|---|---:|---:|---:|---:|---:|
| Server avg GB/s | **1.87** | 1.87 | 0.07 | 1.79 | 1.97 |
| Server peak GB/s | **4.00** | 4.00 | 0.33 | 3.59 | **4.51** |
| Client elapsed (s) | 5.25 | 5.27 | 0.09 | 5.12 | 5.40 |
| Implied client GB/s | mean **2.85** | — | — | — | best **2.93** |

**Per-position bias (healthy runs):** c0 sustained 1.94 GB/s vs c1 1.81 GB/s (Δ +0.13 GB/s in c0's favour); peak is essentially tied (~4.00 GB/s for both). The bias is *sustained, not peak* — likely the 100 ms client-spawn stagger letting c0 prime its pipeline first. If you change the stagger, expect this to move.

**Trends to remember:**
- **Steady-state GB/s is tight** — stdev/mean = 4% on healthy runs. The pipeline is genuinely stable; variance lives at the edges (peaks + tail starvations).
- **Tail starvation in 3/10 runs.** Pre-existing scheduler/wakeup fairness bug in `BluefinServer` (one conn's recv task starves on the server). Not in the perf path; tracked under live bottleneck #11 and scheduler work.
- **Best-case implied client throughput is ~2.93 GB/s** when no starvation. With perfect bilateral delivery, two healthy conns sharing one machine could plausibly climb to ~3 GB/s sustained per conn — the next big lever isn't more CPU work per byte, it's killing the tail.
- **Use peak for ceiling claims, healthy-avg for sustained claims.** The all-runs `avg` (1.75 GB/s) is dragged down by starvation; the healthy-only `avg` (1.87 GB/s) is the honest "what does a working connection deliver" number.

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

