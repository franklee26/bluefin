# Sans-IO migration experiment — post-mortem

**Status:** paused 2026-05-17. Hot-path changes reverted. Wire-format extraction kept.
**Branch with full attempt preserved:** `frank/sans-io-experiment-archive` (also archived as patches in `/tmp/sans-io-archive/` at revert time).
**PR:** #47 (force-pushed down to the kept-only state at revert time).

---

## TL;DR

We tried to migrate Bluefin to a `quinn-proto`-style sans-IO architecture (pure synchronous protocol state machines in `bluefin-proto/`, runtime adapter in `bluefin/`). Roughly half of the migration landed before per-conn throughput regressed materially:

| Stage | Per-conn median sustained | Topology |
|---|---|---|
| Pre-experiment (`origin/main` post rounds O+F1+F2+G3+F3+P) | **~2.25 GB/s** (post-revert re-measured 2026-05-17, 18/20 healthy) | reader / writer / conn_reader / ack-consumer tasks per conn |
| Slice 3 mid-experiment (RX hot-path collapsed behind `Arc<Mutex<ConnectionAdapter>>`) | ~2.36 GB/s avg pre-collapse → measured −16 % vs that → ~1.98 GB/s | single mutex per conn, original task layout |
| Slice 5C (driver task — single `tokio::select!` task owns `Connection` by value) | **~1.82 GB/s** sustained, sometimes 1.75; peak smoothed; bench startup-burst made N≥4 unrunnable on the rcvbuf cliff | one task per conn |

The revert restores the original topology and the original throughput. Two pieces of the experiment were kept because they are non-invasive and stand on their own:

1. **Wire-format extraction into `bluefin-proto/`** — `BluefinHeader`, `BluefinPacket`, `PacketType` moved from `bluefin/src/core/{header,packet}.rs` to [`bluefin-proto/src/wire/`](../bluefin-proto/src/wire/). `bluefin` re-exports them under `bluefin::core::*` for source-compat. A guardrail test ([`bluefin-proto/tests/no_io_deps.rs`](../bluefin-proto/tests/no_io_deps.rs)) fails the build if anyone adds `tokio` / `mio` / `bluefin-io` to the proto crate.
2. **`Endpoint` (handshake demux + hello queue) and `CloseFsm`** — pure sans-IO state machines in [`bluefin-proto/src/endpoint.rs`](../bluefin-proto/src/endpoint.rs) and [`bluefin-proto/src/connection/close.rs`](../bluefin-proto/src/connection/close.rs). The runtime adapters live in [`bluefin/src/net/close_handler.rs`](../bluefin/src/net/close_handler.rs) etc. and own the `Notify` / `Waker` / `AtomicBool` plumbing. This is the "Event-returning FSM + thin runtime wrapper" pattern documented in [bluefin-architecture §5](../skills/bluefin-architecture/SKILL.md).
3. **`bluefin-io` socket factory** — `UdpSocket` construction consolidated in [`bluefin-io/src/socket/factory.rs`](../bluefin-io/src/socket/factory.rs). Cosmetic.

Everything else (data-path `Connection` FSM, `AckFsm`, `RecvFsm`, `SendFsm`, `OrderedBytes` in proto, `ConnectionAdapter` mutex collapse, the single-task driver, busy-poll inner loop, batched recv channel, listener-trace diagnostics) was dropped.

---

## Why the experiment was paused

The proximate cause was a per-conn throughput regression we could not close. The deeper cause was structural and worth recording so the next attempt avoids the same trap.

### What we proved about the regression

- **Slice 3 (`Arc<Mutex<ConnectionAdapter>>`)** measured −16 % on the per-conn benchmark. Contention was real but limited because each conn has its own mutex and exactly one reader + one writer + one ack-consumer competing for it. Not a deal-breaker on its own.
- **Slice 5C (driver task)** collapsed those three tasks per conn into one `tokio::select!` task that owns `Connection` by value (the `!Sync` invariant added precisely to escape the mutex). Sustained per-conn throughput fell to ~1.82 GB/s — about **−23 % vs slice-3 / −35 % vs the documented pre-migration `bench_two_process` runs** that had peaks of 3.85 GB/s.

The flamegraph on the slice-5C topology showed 31.8 % `__recvfrom` + 31.2 % `__sendto` + 21.5 % `park_internal` + 11.0 % `kevent`, with under 3 % spent in protocol code. The kernel was the bottleneck, but specifically it was *kernel work serialized inside a single per-conn task* — `recvfrom` and `sendto` could not overlap on the same connection because they were both gated on the same task's poll cycle. The pre-experiment topology had a *dedicated* writer task per conn that pumped `sendto` at full rate while the reader task pumped `recvfrom` independently.

We tried twice to recover that overlap by offloading just the socket-send to a second task per conn (a "TX-task split" without going all the way to a `RecvHalf`/`SendHalf` protocol split). Both attempts regressed sustained throughput further and broke graceful close. Root causes documented in [`/memories/rust-tokio.md`](https://-) (`mpsc::send` is not wake-free during a burst when the receiver is in `recv_many.await`; graceful-close FIN can race the spawned task's drain). A correct attempt would need a lock-free SPSC ring with a `Notify` that fires only on empty→nonempty plus an explicit finalize barrier — a real refactor we didn't pursue.

A measurement at N=4 / N=5 connections confirmed the ceiling is per-conn, not per-system: surviving conns at N=4 still measured ~1.84 GB/s each, identical to N=2. Adding conns did not add aggregate throughput, because each conn's single driver task still serialised its own RX and TX. (N≥4 also tripped a separate startup-burst → macOS-UDP-rcvbuf-overrun → unfillable-hole cliff which the lossless recv FSM cannot recover from.)

### The right shape (not pursued)

A `RecvHalf` / `SendHalf` split of `Connection` itself — each half is `!Sync`, each owned by its own task, communicating over a single bounded SPSC channel of `RxToTxEvent { AckNeededFor, PeerFinObserved, PeerFinAckObserved }` plus shared close atomics. This restores the per-conn `recvfrom` || `sendto` overlap that the original three-task topology had, without the slice-3 mutex. Detailed plan was scoped but not implemented; see the archive branch's chat history if a future attempt picks it up.

---

## What was kept vs reverted

### Kept (on `frank/sans-io-enforcement` post-revert)

| Commit | Subject | Why kept |
|---|---|---|
| `fadd9bb` (cherry-picked) | first sans pass | Wire-format extraction is a clean dependency win — `bluefin-proto` is a real crate now, the runtime still works the same way. Adds `Endpoint` + `CloseFsm` which the runtime *does* use (close-handler wraps `CloseFsm`). |
| `7100c09` (cherry-picked) | slice 4 (minimal) — socket factory consolidated into `bluefin-io` | Cosmetic move. Used by the runtime. |

### Reverted

| Commit | Subject | Why reverted |
|---|---|---|
| `a8a1471` | slices 3c/3d/3b-TX — `Connection` FSM + `AckBuffer` wrap + TX packetiser to proto | Adds proto FSMs (`conn`, `ack`, `recv`, `send`, `ordered_bytes`, `sliding_window`) that were only useful when wired in by slice 3 / slice 5. Without the driver, dead code. User asked: drop dead code. Available on the archive branch if revived. |
| `5981dd1` | slice 3 — RX hot-path lock collapse to unified `ConnectionAdapter` | Start of −16 % perf regression. |
| `f605792` | slice 5 phase A — send-side API to proto `Connection` | Built on `a8a1471`'s `conn.rs`; gone with it. |
| `e8bce23` | "another batch" — slice 5C driver-task rewire | Brought sustained throughput to 1.82 GB/s. Single-task-per-conn serialised `recvfrom`/`sendto`; structural cost we could not close without further refactor. |
| `7938c8b`, `8345739` | docs commits | Stale after the code revert. |
| in-session WIP (busy-poll inner loop, batched `Vec<Bytes>` recv channel, `BLUEFIN_DIAGNOSTICS` env gate, listener-reader trace counters) | various | All in the driver world. Captured in `/tmp/sans-io-archive/` patches in case any piece is portable back. The listener-trace counter confirmed `SO_REUSEPORT + connect()` routes correctly on macOS post-handshake (2 hellos + 3 data datagrams over 20M-datagram bench) — useful prior knowledge for any future work. |

---

## Lessons (for whoever picks this up next)

1. **A sans-IO protocol crate is not free.** The `Event`-returning-FSM + runtime-wrapper pattern works fine for slow paths (handshake, close). On the data path, the per-packet round-trip through an FSM call + an event-handling adapter has measurable overhead even when the FSM is pure synchronous code. We didn't isolate that overhead from the topology cost in this attempt; the next attempt should A/B-test the FSM cost *with the original task topology* before touching task layout.
2. **Do not collapse a multi-task per-conn pipeline into one task on a kernel-bound workload.** macOS UDP loopback at >2 GB/s spends ~60 % of its time in `__recvfrom` + `__sendto`. If those calls are interleaved on a single task, the kernel's per-direction concurrency is wasted. The slice-3 mutex contention is a smaller problem than the slice-5C task fusion.
3. **`tokio::sync::mpsc::send` wakes the receiver on every send when the receiver is parked.** It is not the empty→nonempty-only signal it looks like. Verified twice in TX-split attempts. If you need wake-free SPSC, use a lock-free ring + `Notify` fired explicitly on the transition, or `crossbeam_channel` from a `spawn_blocking` thread. Recorded in [`/memories/rust-tokio.md`](https://-).
4. **Offloading the last step of a graceful close to a spawned task creates a finalize race.** The originating task can drop its sender and signal close-ack before the spawned task has drained the queued FIN. Mitigation requires a join barrier (a oneshot ack from the spawned task or a `JoinHandle::await`). Both TX-split attempts hit this; the second one timed out 20/20 close attempts in the bench.
5. **macOS dtrace flamegraphs are shallow.** Callers above kernel and runtime frames are routinely dropped. Only inclusive percentages are reliable. Don't infer call sites from leaf-symbol patterns; verify against the code.
6. **Bench harness has a startup-burst cliff at N≥4.** Multiple conns handshaking near-simultaneously overrun the macOS clamped 8 MB UDP rcvbuf during the handshake-to-stream handoff. The lossless recv FSM has no retransmit, so one drop = unfillable hole = silent stall. `--stagger 0.5` partially mitigates; `--stagger 2` is *worse* (it lets the second conn's bulk phase overlap the first's). True fix would be receiver-side hole-skip or a baked-in client-side throttle ramp.
7. **Don't reset retry budget per attempt on hosted CI runners.** (Adjacent lesson from the same period; recorded in [`/memories/hosted-ci-perf-gates.md`](https://-).)

---

## Where to look

- Archive branch: `frank/sans-io-experiment-archive` (full pre-revert tip with all in-session WIP committed).
- Patches: `/tmp/sans-io-archive/0001-…`-through-`0009-…` (`git format-patch origin/main..frank/sans-io-experiment-archive`).
- Current branch tip (`frank/sans-io-enforcement`): just `fadd9bb` + `7100c09` on top of `origin/main`.
- Skill files that still reflect the kept work: [bluefin-101](../skills/bluefin-101/SKILL.md) (proto crate layout), [bluefin-architecture §3 + §5](../skills/bluefin-architecture/SKILL.md) (kept-slices summary, FSM + runtime-wrapper pattern), [bluefin-performance](../skills/bluefin-performance/SKILL.md) (unchanged — it documents the original topology that the revert restored).
