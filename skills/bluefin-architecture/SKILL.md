---
name: bluefin-architecture
description: System shape and design rationale for the Bluefin reference implementation. Covers the three-crate split, the dual-socket model (one listener for handshake + one connected socket per data connection), the connection-demux table, the buffer-with-waker cross-task synchronisation primitive, the per-connection task topology, and the threading/affinity model. Explains *why* the system looks the way it does and what invariants the layout depends on. Pair with `bluefin-protocol` for the wire format and with `bluefin-101` for "where does this live in the code". Load this when proposing structural changes (new task, new socket, replacing the buffer/waker pattern, multi-path) or when authoring RFC sections that overlap implementation choice.
---

# Bluefin Architecture

> **Status**: pre-RFC, single-implementation. This document describes the reference architecture; the protocol itself ([bluefin-protocol](../bluefin-protocol/SKILL.md)) does not mandate this shape.

## 1. Scope

This skill answers **"why is the system shaped this way?"**, not **"where is the code?"** (that's [bluefin-101](../bluefin-101/SKILL.md)) or **"what bytes go on the wire?"** (that's [bluefin-protocol](../bluefin-protocol/SKILL.md)).

Read this first when:

- Adding a new task type or restructuring an existing one.
- Replacing a synchronisation primitive (mutex, channel, waker).
- Considering multi-path, connection migration, or any change to the socket layout.
- Drafting an RFC section that touches "how to implement" guidance.

Don't read this for routine bug fixes or perf tuning of an existing path — the relevant local context is in [bluefin-101](../bluefin-101/SKILL.md) and [bluefin-performance](../bluefin-performance/SKILL.md).

## 2. Crate layering

Three crates in one workspace; the dependency arrows point one way.

```
bluefin            (public API, workers, connection mgmt, ordered-byte buffer)
   │
   ├── bluefin-proto      (BluefinError, BluefinHost enum, handshake state machine)
   │
   └── bluefin-io         (raw UDP socket abstraction, optional vectorised I/O)
                          [currently NOT wired into the runtime]
```

Rationale:

- **`bluefin-proto`** holds protocol-level types that are useful to *anything* speaking Bluefin — including a future alternative implementation, a wireshark dissector, or a fuzzer. Keeping `BluefinError` and `BluefinHost` here means consumers don't need the runtime crate to talk about errors or roles.
- **`bluefin-io`** is a deliberate split for performance work that may break out of `tokio::net::UdpSocket` (vectorised `recvmmsg`/`sendmmsg` on Linux, `recvmsg_x`/`sendmsg_x` on macOS). Today it's unused at runtime — `bluefin/` still goes through `tokio::net::UdpSocket` directly. The split exists so the eventual switch is a dependency change in `bluefin/`, not a rewrite.
- **`bluefin/`** is everything else: connection management, the worker tasks, the ordered-byte buffer, the public client/server types.

## 3. Dual-socket model

Bluefin uses **two distinct UDP sockets per connection** at each peer:

| Socket | Owner | Purpose | Lifetime |
|--------|-------|---------|----------|
| Listener | `BluefinServer` (and the ephemeral one on `BluefinClient`) | Receives handshake datagrams. Demuxed to per-connection buffers via `ConnectionManager`. | Server: lifetime of the host. Client: created per `connect()` call. |
| Per-connection | `BluefinConnection` | Sends + receives **data and ack** for one connection only. `connect()`-ed to the peer's address so the kernel does the demux. | Lifetime of the connection. |

The handshake exchange (see [bluefin-protocol §6](../bluefin-protocol/SKILL.md#6-three-way-handshake)) flows through the listener socket and the `ConnectionManager` table. As soon as the handshake completes, both sides spin up a fresh per-connection socket and route all subsequent data + ack packets through that. The listener socket then sees only handshake traffic for the rest of the connection's life.

### Why two sockets

- **Kernel-side demux for data path.** A `connect()`-ed UDP socket only delivers datagrams from the connected peer, so the per-connection reader needs no userspace lookup. This is the recv-side counterpart to a single dedicated `mpsc` channel per connection: no contention, no map lookup, no branching on connection ID.
- **Listener stays simple.** It only ever sees handshake packets, so its hot loop can be optimised for low rate / large fan-out (many connections handshaking) rather than for sustained throughput.
- **Failure isolation.** A blocked connection's per-conn socket back-pressures only that connection; it doesn't slow handshake on the listener.

### Cost / known limitation

Per [bluefin-performance](../bluefin-performance/SKILL.md) live bottleneck #6: the per-connection `connect()`-ed socket defeats `SO_REUSEPORT`-based recv-side fan-out across cores. On Linux this caps recv parallelism to a single epoll registration per connection. An alternative architecture would route everything through a single `SO_REUSEPORT`-bound listener and demux in userspace; that would trade kernel-side filtering for userspace concurrency. **Open RFC question** — whether to mandate one model.

## 4. Connection demux table

`ConnectionManager` is a `dashmap::DashMap<(u32, u32), ConnectionManagedBuffers>` ([`net/connection.rs`](../../bluefin/src/net/connection.rs)) that maps a directed `(local_id, peer_id)` pair to the buffers belonging to that connection. Every `BluefinHost` (server or client) owns one.

Properties:

- **Lock-free for reads**, sharded mutex for writes (DashMap default). Read-heavy workload: every received handshake datagram does a lookup; every `accept()`/connection-finalise does an insert.
- **Single source of truth**: the listener-socket reader (`ReaderTxChannel`) consults this map to figure out which `ConnectionBuffer` to drop a parsed handshake packet into.
- **Handshake hello queue**: during step 1, the server doesn't yet know the client's `src_conn_id`, so it stores the in-progress accept under `(server_chosen_id, 0)`. After step 3 it removes the placeholder and reinserts under `(server_chosen_id, client_id)`. A shared `HelloState` struct ([`net/mod.rs`](../../bluefin/src/net/mod.rs)) holds both `pending_accept_ids` (FIFO `VecDeque` of accept slots) and a bounded `hello_queue` (`VecDeque` of pre-arrived `ClientHello` packets, capped at 64). The `ReaderTxChannel` and `accept()` coordinate under a single mutex — hellos arriving before an accept slot are queued, and `accept()` drains the queue before blocking.
- **Close-time deregistration**: [`BluefinConnection::close`](../../bluefin/src/net/connection.rs) removes its `(src, dst)` entry from the `ConnectionManager` once the FIN / FIN-ACK exchange completes (or after the retransmit budget is exhausted). After that point the listener-socket reader drops any stray packets for the connection — but stray packets shouldn't reach the listener anyway, since the per-conn data socket owns the data path (§3) and only the FIN / FIN-ACK exchange itself runs through it.

The per-connection data socket bypasses this table entirely — that's the whole point of §3.

## 5. Buffer-with-waker pattern

This is the cross-task synchronisation primitive used everywhere in Bluefin. It deserves its own section because it appears in three places (`ConnectionBuffer`, `AckBuffer`, `HandshakeConnectionBuffer`) and any new buffer type SHOULD follow the same pattern.

### Shape

```text
struct SomeBuffer {
    state: <whatever this buffer holds>,
    waker: Option<Waker>,
}

impl SomeBuffer {
    fn buffer_in(&mut self, item: T) -> Result<(), Err> { ... }   // producer
    fn consume(&mut self) -> Option<U> { ... }                    // consumer probe
    fn set_waker_if_changed(&mut self, w: &Waker) { ... }         // poll-side
}

// Shared trait \u2014 one method, see `bluefin/src/net/mod.rs::Wakeable`
impl Wakeable for SomeBuffer {
    fn take_waker_clone(&self) -> Option<Waker> { self.waker.clone() }
}
```

The buffer is `Arc<Mutex<SomeBuffer>>`. The consumer is a `Future` whose `poll`:

1. Locks the mutex.
2. Calls `consume()` — if `Some(_)`, return `Poll::Ready`.
3. Otherwise calls `set_waker_if_changed(cx.waker())`, drops the guard, returns `Poll::Pending`.

The producer (a worker task that just received a packet):

1. Locks the mutex.
2. Calls `buffer_in(packet)`.
3. Calls `take_waker_clone()` to grab a `Waker` (cheap atomic refcount bump on the inner Arc).
4. **Drops the mutex guard.**
5. Calls `wake()` on the cloned waker.

### Why this shape

- **Single mutex per buffer.** Both producer and consumer go through the same `Mutex<SomeBuffer>`. State and the rendezvous waker are colocated, so there's no second sync primitive (no condvar, no atomic flag, no separate channel).
- **`set_waker_if_changed` avoids waker clones on hot re-polls.** Tokio re-polls a future on the same task across many wake-ups; comparing with `Waker::will_wake` skips the atomic refcount bump on the inner waker Arc when the same task is re-arming.
- **Wake-after-drop is mandatory.** Earlier versions of the producer side called `wake_by_ref()` while still holding the buffer's mutex, causing the woken consumer task to immediately try to `lock()` and bounce. This was fixed in 2026-05/#7 — see [bluefin-performance](../bluefin-performance/SKILL.md) historical timeline. Any new buffer type MUST clone the waker out, drop the guard, then wake.

### When NOT to use this pattern

- If the data is "many small messages" and ordering doesn't matter, prefer an mpsc channel — Tokio's already does this internally and you get backpressure for free.
- If only one task ever produces and one ever consumes, an mpsc with capacity 1 is simpler.

The buffer-with-waker pattern earns its keep when the producer's data-shape is **non-FIFO** (e.g. `OrderedBytes` is a sparse circular buffer indexed by packet number) or when the consumer wants to **probe** the buffer rather than receive an item (e.g. `consume()` returns `Option<U>` based on whether enough bytes have arrived to satisfy a read).

## 6. Task topology

Per **host** (server or client process), the runtime owns:

- **`num_reader_workers` × `ReaderTxChannel`** ([`worker/reader.rs`](../../bluefin/src/worker/reader.rs)) bound to the listener socket. Demuxes handshake datagrams into the right `ConnectionBuffer` via `ConnectionManager`. Default: 1. Increase only if handshake rate is the bottleneck (it never has been).

Per **connection**, the runtime additionally spawns:

- **One `ConnReaderHandler`** ([`worker/conn_reader.rs`](../../bluefin/src/worker/conn_reader.rs)) on the per-connection socket. This itself spawns N `tx_impl` tasks (1 on macOS, `available_parallelism()` on Linux) that `recv` from the connected socket and forward parsed packets via mpsc to one `rx_impl` task that buffers them.
- **One `WriterHandler`** ([`worker/writer.rs`](../../bluefin/src/worker/writer.rs)) with two children:
  - `read_data` task → spawned sender task (two-hop: see [bluefin-performance](../bluefin-performance/SKILL.md) for the ~+8.6 % rationale, and **don't** unify them — the obvious `tokio::select!` rewrite was tried and reverted).
  - `read_ack` task: inline `try_send`, no second hop.
- **One `AckConsumer`** ([`net/ack_handler.rs`](../../bluefin/src/net/ack_handler.rs)). Currently dead weight: wakes on every ack batch and writes `largest_recv_acked_packet_num` into an `AtomicU64` that nothing reads. Kept as the future hook for retransmission; remove it if retransmission ends up living elsewhere.

### Why "task per concern" rather than "task per CPU"

Bluefin uses Tokio's multi-threaded scheduler. Tasks are cheap; threads are not. Splitting on **what work needs to happen** rather than on **how many cores are available** lets the scheduler pack work onto cores based on observed contention. The Linux-only `tx_impl` fan-out (N tasks on the recv side) is the only place where we explicitly multiply by core count, and it only works because `SO_REUSEPORT` actually fans recvs across cores on Linux (it doesn't on Apple Silicon — see the comment in [`worker/conn_reader.rs::get_number_of_tx_tasks`](../../bluefin/src/worker/conn_reader.rs)).

## 7. Threading and affinity

- Tokio's multi-threaded runtime is the default ([`bluefin/src/bin/{client,server}.rs`](../../bluefin/src/bin/) use `#[tokio::main(flavor = "multi_thread")]`).
- A `set_current_thread_affinity(cpu_id)` helper exists in [`utils/cpu_affinity.rs`](../../bluefin/src/utils/cpu_affinity.rs). On macOS it uses `thread_policy_set` with `THREAD_AFFINITY_POLICY` (which sets a co-scheduling tag, not a hard pinning); on Linux it's a stub that returns `Unsupported`.
- **It is never called from production code** — only from its own test. Treat it as scaffolding for future work, not a load-bearing component. If you intend to wire it in, do it with a measurement, because Tokio's work-stealing scheduler typically beats hand-pinning for I/O-bound workloads.

## 8. Backpressure and reliability today

A snapshot of what Bluefin actually guarantees right now (most of these will need to be specified properly in the RFC):

| Concern | Today's behaviour | Notes |
|---------|-------------------|-------|
| Send-side backpressure | Bounded `flume::bounded(4096)` between user `send_bytes_async` and the writer pump (~6 MiB cap). Sync `send_bytes` returns `WriteError` on full. | Added 2026-05/#10 as `tokio::sync::mpsc`; swapped to `flume` in 2026-05/P to eliminate per-32-message list-block alloc/free churn. Implementation choice; protocol doesn't mandate. |
| Recv-side backpressure | `OrderedBytes` has `MAX_BUFFER_SIZE` slots; `buffer_in_bytes` returns `BufferFullError` when full. The producer drops the packet on error. | This is **lossy under sustained pressure** — there's no on-wire signal back to the sender. |
| Retransmission | None. The receiver tracks ack acceptance in a `SlidingWindow`; the sender writes acks but never consumes them for retransmit decisions. | The dead `AckConsumer` is the planned hook. |
| Congestion control | None. The send loop runs as fast as `socket.try_send` will let it. | Will need its own RFC section. |
| Connection close | Explicit `Fin` / `FinAck` exchange (bluefin-protocol §10bis). [`BluefinConnection::close`](../../bluefin/src/net/connection.rs) flushes, sends a `Fin`, awaits `FinAck` (200 ms × 3 retransmit budget), and deregisters from the `ConnectionManager`. On the receive side, `recv_bytes` returns `Ok(0)` once the peer's `Fin` is observed and the data buffer is drained — the conn_reader auto-emits the `FinAck`. | The bench server's recv-idle timeout is retained as a *safety net* for crashed/SIGKILL'd peers only. |
| Flush | Public `BluefinConnection::flush().await` waits until every previously-enqueued payload has been written by the writer task. Added 2026-05/D after the bench client's exit-sleep workaround proved fragile. Also called internally as the first step of `close()`. | Implementation only — protocol carries no flush packet. |

## 9. Known architectural debt

The big load-bearing assumptions that may have to change before / during RFC standardisation:

| # | Issue | Where |
|---|-------|-------|
| 1 | ~~**Hello-buffering race**~~ — **FIXED.** `ClientHello` packets arriving before `accept()` are now queued in a bounded `HelloState.hello_queue` (cap 64) and drained by `accept()`. Client-side stagger workarounds removed. | [`net/mod.rs`](../../bluefin/src/net/mod.rs), [`net/server.rs`](../../bluefin/src/net/server.rs), [`worker/reader.rs`](../../bluefin/src/worker/reader.rs). |
| 2 | **Per-connection `connect()`-ed socket** defeats `SO_REUSEPORT` recv-side fan-out. Caps recv parallelism per connection. See §3. | [`net/connection.rs`](../../bluefin/src/net/connection.rs). |
| 3 | **`AckConsumer` is dead** — wakes constantly, writes a value nobody reads. Retransmission needs to either consume that signal or replace the consumer entirely. | [`net/ack_handler.rs`](../../bluefin/src/net/ack_handler.rs). |
| 4 | **`BluefinHost::PackFollower` is reserved but unused.** The protocol name "Bluefin" implies a multi-path / follower-leader topology that has never been built. The RFC has to decide whether to keep it as v1 scope or punt to v2. | [`bluefin-proto/src/context.rs`](../../bluefin-proto/src/context.rs). |
| 5 | ~~**No flush API.**~~ **FIXED** by `BluefinConnection::flush()` (2026-05/D) and `BluefinConnection::close()` (2026-05/§10bis). Clean shutdown is now expressible by the user. | [`net/connection.rs`](../../bluefin/src/net/connection.rs). |
| 6 | **`bluefin-io` is unused at runtime.** Vectorised `recvmsg_x`/`sendmsg_x` exist but `BluefinSocket` is wired only into tests. The whole crate is currently dead weight. | [`bluefin-io/src/socket/udp_socket.rs`](../../bluefin-io/src/socket/udp_socket.rs). |
| 7 | **CPU-affinity helper is unused.** See §7. | [`utils/cpu_affinity.rs`](../../bluefin/src/utils/cpu_affinity.rs). |

## 10. Topics an RFC has to make architectural choices on

These are choices an RFC can't punt to "implementation detail" because they leak through to interoperability or behaviour visible to applications:

- **Per-connection socket vs single demuxed listener.** §3 picks one; an RFC must mandate or allow both.
- **Hello queue depth and timeout.** The reference implementation now buffers up to 64 hellos. The RFC should specify whether this bound is mandatory and what the expiry policy is.
- **Multi-path / pack-follower.** In or out for v1.
- **Flush / close semantics.** What does "the application has finished sending" mean on a stream protocol? The reference implementation answers: `flush()` drains the writer queue, `close()` issues `Fin` + awaits `FinAck`, peer's `recv` returns `Ok(0)` once observed. An RFC has to formalise this state machine — see bluefin-protocol §10bis.
- **Retransmission location.** Per-connection? Per-stream (if streams ever land)? Driven by ack consumer or by sender-side timer?
- **Concurrency model expectations.** The RFC SHOULD avoid mandating threads/tasks, but it MAY require that an implementation can sustain N concurrent connections without head-of-line blocking — and that's a structural claim the architecture has to back.

---

**See also**: [bluefin-protocol](../bluefin-protocol/SKILL.md) for the wire format, [bluefin-101](../bluefin-101/SKILL.md) for the code map, [bluefin-performance](../bluefin-performance/SKILL.md) for measured numbers and the live bottleneck list.
