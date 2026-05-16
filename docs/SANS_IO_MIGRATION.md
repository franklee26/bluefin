# Bluefin sans-io migration plan

> Status: in progress. Slices 0, 1, and 2 have landed; slices 3, 3b, 4,
> 5, 6 are still ahead. Companion to
> [`skills/bluefin-architecture/SKILL.md`](../skills/bluefin-architecture/SKILL.md)
> and [`skills/bluefin-protocol/SKILL.md`](../skills/bluefin-protocol/SKILL.md).
> Cross-reference: [`THROUGHPUT_ANALYSIS_2026.md`](../THROUGHPUT_ANALYSIS_2026.md)
> (perf invariants we must not break) and
> [`skills/bluefin-ci/SKILL.md`](../skills/bluefin-ci/SKILL.md) (gates this
> migration must keep green).

## 0. Progress log

| Slice | Status | Landed | Notes |
|-------|--------|--------|-------|
| 0 — guardrail | ✅ done | 2026-05-15 | [`bluefin-proto/tests/no_io_deps.rs`](../bluefin-proto/tests/no_io_deps.rs) parses `bluefin-proto/Cargo.toml` and fails if any of `tokio`, `mio`, `async-std`, `smol`, `futures`, `tokio-util`, or `bluefin-io` appears. |
| 1a — wire types into `bluefin-proto::wire` | ✅ done | 2026-05-15 | `BluefinHeader`, `PacketType`, `BluefinPacket`, `Serialisable`, `Extract` moved to [`bluefin-proto/src/wire/`](../bluefin-proto/src/wire/). [`bluefin/src/core/mod.rs`](../bluefin/src/core/mod.rs) is a re-export shim for source-compat. Zero-copy `Bytes`-backed payload preserved via `from_bytes_into`. |
| 1b — `Endpoint` (hello-queue FSM) | ✅ done | 2026-05-15 | [`bluefin-proto/src/endpoint.rs`](../bluefin-proto/src/endpoint.rs) holds `pending_accept_ids` + bounded `hello_queue` (cap 64) and exposes `take_queued_hello_or_register` / `classify_hello → HelloOutcome { NotHello, Routed, Queued, Dropped }`. Dead `HandshakeHandler` stub deleted from [`bluefin/src/net/client.rs`](../bluefin/src/net/client.rs). [`bluefin/src/worker/reader.rs`](../bluefin/src/worker/reader.rs), [`net/server.rs`](../bluefin/src/net/server.rs), and [`net/client.rs`](../bluefin/src/net/client.rs) now hold `Arc<Mutex<Endpoint>>` in place of the old `HelloState`. |
| 2 — close FSM (`CloseFsm` + `CloseEvent`) | ✅ done | 2026-05-16 | Pure state machine in [`bluefin-proto/src/connection/close.rs`](../bluefin-proto/src/connection/close.rs). Returns `CloseEvent { None, PeerFinObserved, PeerFinAckObserved }` so the adapter performs cross-task wakes outside the FSM lock. [`bluefin/src/net/close_handler.rs`](../bluefin/src/net/close_handler.rs) is now a thin wrapper that owns the runtime surfaces (`Arc<AtomicBool>` for the lock-free EOF mirror, `Arc<Notify>` for `close().await`, `Option<Waker>` for the recv data buffer). Public API preserved → no changes to `conn_reader`, `connection`, `client`, `server`. |
| 3 — RX data path (`SlidingWindow` + `OrderedBytes` into `Connection`) | ⏳ next | — | First slice that mutates the data hot path. Will trigger the documented perf regression that slice 6 recovers. |
| 3b — TX data path (packetisation, packet-num alloc, ack/data interleave) | ⏳ pending | — | |
| 4 — `bluefin-io` becomes the only socket owner | ⏳ pending | — | |
| 5 — runtime adapter shrinks | ⏳ pending | — | |
| 6 — perf re-tune | ⏳ pending | — | Mandatory; gates the migration's "done". |

_Per the §8 acceptance checklist: each landed slice has its own
`cargo test --workspace` + clippy green and added at least one
state-machine-isolation test (no Tokio runtime). The §5 prose below
describes the original plan; minor naming adjustments made during
implementation are reflected in the table above._

## 1. Goal and definition of "sans-io"

A sans-io implementation is one in which the protocol logic — handshake,
packetisation, ack scheduling, ordering/reassembly, retransmit timing,
graceful close, congestion control — is a pure synchronous state machine
that:

1. Takes inputs as plain values: bytes received from the network, the
   current monotonic time, app-level send/recv calls, timer fires.
2. Emits outputs as plain values: bytes to send on the network, timer
   updates, app-level events (data ready, connection closed, error).
3. Owns no sockets, spawns no tasks, and never `await`s.

The reference implementation we are aiming at is
[`quinn-proto`](https://github.com/quinn-rs/quinn/tree/main/quinn-proto):
`Endpoint::handle()`, `Connection::handle_event()`,
`Connection::poll_transmit(now, max_datagrams)`,
`Connection::poll_timeout()`, `Connection::poll()`. The async runtime
adapter (`quinn`) is a thin shim that wires those calls to a Tokio
`UdpSocket` and timer.

Bluefin today is the opposite: protocol logic lives inside the worker
tasks that own the sockets. The split crates exist but they are
near-empty scaffolding — see §3.

## 2. Why this matters here (concretely)

- **`bluefin-proto::handshake::HandshakeHandler`** is currently a
  no-op. `handle()` returns `Ok(())`; `Transmit` and `PendingAccept`
  carry only random IDs. The single embedder
  ([`bluefin/src/net/client.rs:33`](../bluefin/src/net/client.rs))
  holds it as an unused field. The migration's job is to make this
  type real.
- **`bluefin-io::BluefinSocket`** is wired only into its own tests.
  The runtime still calls `tokio::net::UdpSocket::recv_from` /
  `try_send` directly from
  [`worker/reader.rs`](../bluefin/src/worker/reader.rs),
  [`worker/conn_reader.rs`](../bluefin/src/worker/conn_reader.rs), and
  [`worker/writer.rs`](../bluefin/src/worker/writer.rs). The
  migration's job is to make the I/O crate the *only* place that
  imports `tokio::net`.
- **Protocol logic is currently entangled with task plumbing.** Every
  one of the items below is a piece of state-machine work that lives
  inside an `async fn` driven by a worker:
  - Handshake decode + state transitions:
    [`worker/reader.rs::ReaderTxChannel`](../bluefin/src/worker/reader.rs)
    + [`net/{client,server}.rs`](../bluefin/src/net/).
  - Per-connection packet parsing, type dispatch, auto-emit FinAck:
    [`worker/conn_reader.rs::ConnReaderHandler::buffer_in_packets`](../bluefin/src/worker/conn_reader.rs).
  - Receive-side ordering and EOF surfacing:
    [`net/ordered_bytes.rs`](../bluefin/src/net/ordered_bytes.rs).
  - Ack window + ack-trigger cadence
    (`packets_consumed_before_ack = 200`):
    [`worker/reader.rs:79`](../bluefin/src/worker/reader.rs),
    [`net/ack_handler.rs`](../bluefin/src/net/ack_handler.rs).
  - Outbound packetisation, packet-number allocation, ack-only datagram
    construction, FIN reservation:
    [`worker/writer.rs`](../bluefin/src/worker/writer.rs),
    [`net/connection.rs`](../bluefin/src/net/connection.rs).
  - Close FSM + FIN/FinAck retransmit budget:
    [`net/close_handler.rs`](../bluefin/src/net/close_handler.rs),
    `BluefinConnection::close`.
  - The (currently dead) ack-consumer hook for future retransmission:
    [`net/ack_handler.rs`](../bluefin/src/net/ack_handler.rs).

  All of these belong, by the sans-io rule, in `bluefin-proto`. None
  of them are there today.

## 3. Target shape

```text
bluefin-proto      — pure state machines, no async, no tokio, no I/O.
   ├── Endpoint        (handshake demux, accepts, connection IDs)
   ├── Connection      (per-conn FSM: handshake, data, ack, close, retransmit)
   ├── Transmit        (datagram-out: dst addr + payload bytes + ecn?)
   ├── Event           (datagram-in, app-send, app-recv-ready, timer-fire)
   ├── AppEvent        (handshake-done, data-ready, peer-closed, errored)
   └── Timer           (single Option<Instant> per connection, à la quinn)

bluefin-io         — UDP only. No protocol knowledge.
   ├── UdpSocket       (recv_from / send_to; abstracts tokio + recvmsg_x)
   └── (future)        batched recv/send via recvmmsg / recvmsg_x

bluefin            — async adapter. Owns sockets + tasks + timers.
                     Drives the proto state machines, exposes the
                     existing public API (BluefinClient, BluefinServer,
                     BluefinConnection) unchanged.
```

The public API (`BluefinClient::new/connect`, `BluefinServer::bind/accept`,
`BluefinConnection::{send,send_bytes,send_bytes_async,recv,recv_bytes,
flush,close,is_closed}`) MUST remain source-compatible across the
migration. All bench harnesses
([`bench_two_process.sh`](../bench_two_process.sh), `bench_ci.sh`)
must continue to build and run unmodified.

## 4. State-machine API sketch

Modelled on quinn-proto. Synchronous, allocation-aware.

```rust
// bluefin-proto/src/endpoint.rs (new)
pub struct Endpoint { /* connection table, accept queue, hello queue */ }

impl Endpoint {
    pub fn new(role: BluefinHost, config: EndpointConfig) -> Self;

    /// Feed a datagram that arrived on the listener socket. Returns
    /// `Some((ConnectionHandle, DatagramEvent))` when the datagram
    /// belongs to / creates a connection. Otherwise the datagram is
    /// internal (e.g. a stray hello) and is consumed.
    pub fn handle(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
    ) -> Option<(ConnectionHandle, DatagramEvent)>;

    /// Drain pending endpoint-level transmits (e.g. ServerHello).
    pub fn poll_transmit(&mut self) -> Option<Transmit>;

    /// App-level: dequeue a fully-handshaken inbound connection.
    pub fn accept(&mut self) -> Option<(ConnectionHandle, Connection)>;

    /// App-level: start an outbound connection.
    pub fn connect(
        &mut self,
        now: Instant,
        remote: SocketAddr,
    ) -> Result<(ConnectionHandle, Connection), ConnectError>;
}

// bluefin-proto/src/connection.rs (new)
pub struct Connection { /* OrderedBytes, SlidingWindow, CloseState, ... */ }

impl Connection {
    /// Feed a datagram that arrived on the per-connection socket (or was
    /// routed here by the Endpoint).
    pub fn handle_datagram(&mut self, now: Instant, data: BytesMut);

    /// App-level: enqueue payload bytes for transmission. Returns the
    /// number of bytes accepted (may be less than `data.len()` under
    /// flow control once that lands).
    pub fn write(&mut self, data: Bytes) -> Result<usize, WriteError>;

    /// App-level: drain ordered bytes into `out`. Returns how many
    /// payload-byte items were appended.
    pub fn read(&mut self, out: &mut Vec<Bytes>, max_packets: usize)
        -> Result<usize, ReadError>;

    /// Pop the next datagram to send on this connection's socket.
    /// `max_payload` is the configured per-datagram cap
    /// (= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM today).
    pub fn poll_transmit(&mut self, now: Instant, max_payload: usize)
        -> Option<Transmit>;

    /// When the runtime should next call `handle_timeout`. `None` means
    /// "no timer needed". The runtime's job is to arm a single sleep.
    pub fn poll_timeout(&self) -> Option<Instant>;

    /// Advance retransmit / ack-delay / close-retry timers.
    pub fn handle_timeout(&mut self, now: Instant);

    /// App-level event drain (data ready, peer closed, etc.).
    pub fn poll_event(&mut self) -> Option<AppEvent>;

    /// App-level: initiate graceful close.
    pub fn close(&mut self, now: Instant);
    pub fn is_closed(&self) -> bool;
}
```

**Invariants we must preserve from §2 of `bluefin-101`:**

- `MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM = 10`,
  `MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM = 15200`.
- All packets in a single datagram belong to the same connection AND are
  the same kind (data XOR ack). Handshake packets are 1-per-datagram.
- Hello queue cap of 64 (architecture §4).
- Close: 200 ms × 3 retransmit budget (architecture §8).
- Ack cadence: 200 packets consumed → trigger ack
  ([`worker/reader.rs:79`](../bluefin/src/worker/reader.rs)). Becomes
  internal state in `Connection`; no longer a magic constant in a worker.

## 5. Migration in slices

Each slice is an independently land-able PR with green tests and CI
gates. **No single PR refactors the data hot path.** That comes after
the boundary exists end-to-end, with a benchmark in front of it.

### Slice 0 — guardrails

- Add `cargo deny`-style import lints (or a unit test) asserting that
  `bluefin-proto/src/**` does not depend on `tokio`, `bluefin-io`, or
  `std::net::UdpSocket`.
- Add the same for `bluefin-io/src/**` against `bluefin-proto`'s
  state-machine modules.
- Stub `docs/SANS_IO_MIGRATION.md` (this file). Update
  `skills/bluefin-architecture/SKILL.md` §10 with a link.

Risk: zero. Outcome: the migration's "no I/O in proto" rule is enforced
at compile time, so subsequent slices can't accidentally regress.

### Slice 1 — handshake state machine

Goal: replace the stub `HandshakeHandler` with a real one and route the
handshake datagrams through it from the existing async workers.

- Move into `bluefin-proto`:
  - `BluefinHeader`, `PacketType`, `BluefinPacket` (currently in
    [`bluefin/src/core/`](../bluefin/src/core/)). These are wire-format
    types; they belong with the protocol, and `bluefin-io` will need
    them too. Re-export from `bluefin/` for source compat.
  - The hello queue (`HelloState`, currently in
    [`net/mod.rs`](../bluefin/src/net/mod.rs)) and the
    `pending_accept` slot allocator. These are pure FIFO/dashmap state,
    no I/O.
- Implement `Endpoint::handle` to:
  - Parse the datagram into a `BluefinPacket`.
  - Dispatch on `PacketType`:
    `UnencryptedClientHello → enqueue or kick pending accept;
     UnencryptedServerHello → drive client FSM;
     ClientAck → finalise server FSM, emit `Accept` AppEvent`.
- Implement `Endpoint::poll_transmit` for the `ServerHello` /
  retransmit-of-`ClientHello` outputs.
- In the runtime
  ([`worker/reader.rs`](../bluefin/src/worker/reader.rs)), replace the
  `match packet.header.type_field` ladder with a single
  `endpoint.handle(now, addr, ecn, bytes)` call, then drain
  `endpoint.poll_transmit()` and `endpoint.accept()`.
- Delete the unused `handshake_handler` field from
  [`net/client.rs:33`](../bluefin/src/net/client.rs).

**Data path is untouched.** Only the listener socket loop changes.
`bench_ci.sh` floors must stay green. Add a `proptest` for
`Endpoint::handle` driven against a recorded handshake transcript so we
can refactor the state machine later without touching tests of the
async layer.

### Slice 2 — close FSM into proto

Goal: lift the close state machine into `Connection`. The per-connection
data socket loop continues to own actual I/O but stops owning protocol
state.

- Move `CloseState`, `pending_fin_ack_send`, `received_fin_ack_for`
  ([`net/close_handler.rs`](../bluefin/src/net/close_handler.rs)) into
  `bluefin-proto::Connection`. The 200 ms × 3 retransmit budget becomes
  state advanced by `Connection::handle_timeout`.
- Replace
  [`worker/conn_reader.rs::buffer_in_packets`](../bluefin/src/worker/conn_reader.rs)
  branch on `Fin/FinAck` with a `connection.handle_datagram(now, bytes)`
  call.
- The async `BluefinConnection::close` becomes a small loop:
  `connection.close(now); flush_writer(); loop { drive_timer().await }`.
- Keep `peer_fin_observed: Arc<AtomicBool>` as a runtime-owned
  signalling shortcut for the recv hot path; `Connection::poll_event`
  returns `AppEvent::PeerClosed` and the runtime sets the bool.

Risk: medium. Close is rare on the bench, so a regression won't show
up in throughput numbers but will break the close-handler tests in
[`bluefin/tests/`](../bluefin/tests/). Run those locally before pushing.

### Slice 3 — ack window + recv ordering into proto (data path, RX side)

Goal: lift `SlidingWindow`
([`net/ack_handler.rs`](../bluefin/src/net/ack_handler.rs)) and
`OrderedBytes`
([`net/ordered_bytes.rs`](../bluefin/src/net/ordered_bytes.rs)) into
`bluefin-proto::Connection`.

- These are already pure-data structures. The lift is mostly a move
  + a re-export. The work is in changing the *interaction*:
  `Connection::handle_datagram` calls `OrderedBytes::buffer_in`
  internally instead of the worker doing it.
- `Connection::read(out, max_packets)` replaces
  [`ReaderRxChannel::read`](../bluefin/src/worker/reader.rs).
- Ack-trigger cadence (`200`) becomes a field of `Connection` and a
  `poll_transmit` decision, not a counter inside an async loop.
- The buffer-with-waker pattern (architecture §5) does NOT cross the
  proto boundary. The state machine returns `AppEvent::DataReady`
  from `poll_event`; the runtime adapter owns the `Waker`. This
  removes `ConnectionBuffer`/`AckBuffer` as cross-task primitives —
  they become plain fields of `Connection`, mutated only by the
  single task driving that `Connection`.
- The dead `AckConsumer` ([architecture §9 #3](../skills/bluefin-architecture/SKILL.md#9-known-architectural-debt))
  goes away; `Connection` owns the ack window directly and is the
  natural home for the future retransmit hook.

This slice touches the data hot path. Per §7, we accept a temporary
throughput regression and re-tune in slice 6.

### Slice 3b — send-side packetisation into proto (data path, TX side)

Goal: lift the writer's protocol responsibilities into `Connection`,
leaving the async writer task as a pure pump.

Currently entangled inside
[`worker/writer.rs`](../bluefin/src/worker/writer.rs):

- Packet-number allocation (`next_packet_num` / `next_packet_num_shared`).
- Header construction + packetisation of user `Bytes` into
  ≤ `MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM` datagrams.
- Ack-only datagram construction from the `AckData` channel.
- The data-XOR-ack invariant per datagram
  ([bluefin-101 §Packet vs Datagram](../skills/bluefin-101/SKILL.md)).
- FIN packet-number reservation (`reserve_close_packet_num`).
- `pending_bytes` / `flush_notify` accounting backing
  `BluefinConnection::flush`.

All of these move behind `Connection`:

- `Connection::write(Bytes) -> Result<usize, WriteError>` accepts
  payload bytes; internally enqueues onto a per-connection send
  queue (today: the `flume::bounded(4096)` channel of `Bytes`).
- `Connection::poll_transmit(now, max_payload) -> Option<Transmit>`
  is the *only* place that allocates packet numbers and builds
  datagrams. It is called both for fresh data and for ack-only and
  FIN/FinAck datagrams; the data-XOR-ack invariant becomes an
  internal `poll_transmit` rule, not an architectural one spread
  across two channels.
- `Connection::poll_flushed() -> bool` (or an
  `AppEvent::Flushed` edge) replaces the `pending_bytes` /
  `flush_notify` pair. The runtime adapter exposes this via
  `BluefinConnection::flush().await`.
- The two-hop writer pipeline (packetise → mpsc → `try_send`,
  documented as +8.6 % in
  [`THROUGHPUT_ANALYSIS_2026.md` §5](../THROUGHPUT_ANALYSIS_2026.md))
  becomes a *runtime* concern. The adapter MAY drain
  `poll_transmit` on a dedicated task that hands off datagrams to a
  send task via mpsc, or call `socket.try_send` inline — both are
  observably equivalent to `Connection`. Pick whichever wins in
  slice 6's re-tune.
- The user-facing `flume::bounded(4096)` send queue stays in the
  adapter. `Connection::write` returns `WriteError::WouldBlock`
  when its internal queue is full, and the adapter translates that
  into either `BluefinError::WriteError` (sync API) or an `.await`
  on a notify (`send_bytes_async`). Backpressure semantics on the
  public API are unchanged.

Slices 3 and 3b can land in either order. Land them as separate
PRs so a regression bisect points at the side that caused it.

### Slice 4 — `bluefin-io` is the only socket owner

Goal: remove every direct `tokio::net::UdpSocket` use from `bluefin/`.

- Promote `bluefin-io::UdpSocket` to the runtime's only socket type.
  Today's two call sites
  ([`worker/reader.rs`](../bluefin/src/worker/reader.rs) listener,
  [`worker/conn_reader.rs`](../bluefin/src/worker/conn_reader.rs)
  per-connection) become its only callers.
- Add `BluefinSocket::recv_batch(&mut [BytesMut])` and `send_batch`
  hooks. On macOS these route to the existing `recvmsg_x`/`sendmsg_x`
  paths (after fixing the documented `Ok(1)`/`Ok(8)` bug — see
  `THROUGHPUT_ANALYSIS_2026.md` §6); on Linux to `recvmmsg`/`sendmmsg`;
  fallback to `tokio::net::UdpSocket::recv_from`/`send_to` elsewhere.
- The runtime adapter glue (the `recv_and_buffer_inline` /
  `tx_impl` / `rx_impl` split in
  [`worker/conn_reader.rs`](../bluefin/src/worker/conn_reader.rs))
  stays as-is; it just calls `bluefin_io::UdpSocket` instead of
  `tokio::net::UdpSocket`. Same task topology, same channel shapes.

This is a swap, not a redesign. Land it independently of slice 3 so
either can ship first.

### Slice 5 — runtime adapter shrinks

Once slices 1–4 land, `bluefin/src/worker/*.rs` is reduced to a thin
adapter per connection plus an endpoint loop:

- One Tokio task per `Connection` that loops:
  ```text
  select! {
      datagram = socket.recv() => connection.handle_datagram(now(), datagram),
      _ = sleep_until(connection.poll_timeout()) => connection.handle_timeout(now()),
      app_call = api_rx.recv() => apply_to(connection, app_call),
  }
  while let Some(t) = connection.poll_transmit(now(), MAX_DATAGRAM) {
      socket.try_send(&t.payload)?; // or hand off to send task (see below)
  }
  while let Some(ev) = connection.poll_event() { dispatch_to_app(ev); }
  ```
- The dual-socket model (architecture §3) survives unchanged: the
  `Endpoint` lives behind the listener socket, each `Connection` lives
  behind a per-connection socket. Sans-io is orthogonal to that split.
- The data/ack channel split inside the writer goes away (it's now an
  internal `poll_transmit` ordering decision). What remains is the
  *optional* send-side two-hop: drain `poll_transmit` on the
  conn-driver task, push datagrams via mpsc to a sender task that does
  `try_send`. Keep or drop based on slice 6's measurements; the proto
  layer is indifferent.

Most of this slice is deletion: `ConnectionBuffer`, `AckBuffer`,
`CloseBuffer`, the diagnostic plumbing around them, and the
buffer-with-waker pattern as a *cross-task* primitive (architecture §5).
The pattern remains valid wherever runtime-only state crosses tasks
(e.g. the user-facing send queue), but the protocol state machines
no longer use it.

### Slice 6 — performance re-tune

Goal: recover the pre-migration loopback throughput
(~3.03 GB/s baseline,
[`THROUGHPUT_ANALYSIS_2026.md`](../THROUGHPUT_ANALYSIS_2026.md)) after
the sans-io boundary is in place. Sequenced *after* correctness lands
so we measure against a stable target.

Measurement protocol:

1. Lock in `bench_two_process.sh` numbers on `main` before slice 3.
   Record raw + good-conn max-peak, mean-avg, and median across
   ≥ 10 runs (the hosted-runner long-left-tail caveats from the
   user-memory `hosted-ci-perf-gates` notes apply).
2. After slices 3, 3b, 4, 5 land, re-run the same protocol on the
   same hardware. Each PR in this slice changes *one* knob and is
   accepted only if it moves median throughput monotonically toward
   the §1 baseline without breaking correctness tests.

Knobs available, ordered by expected impact (highest first):

- **Batched recv/send via `bluefin-io`** (already implemented for tests,
  unwired in runtime). Slice 4 wires the API; slice 6 turns it on.
  Single biggest lever per
  [`THROUGHPUT_ANALYSIS_2026.md` §6](../THROUGHPUT_ANALYSIS_2026.md).
- **Datagram coalescing inside `Connection::poll_transmit`.** The
  state machine already knows packet boundaries; emit one Transmit
  carrying multiple packets up to `MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM`,
  preserving the existing packetisation invariant.
- **Zero-copy carriage for inbound payloads.** Keep the `BytesMut` recv
  buffer alive across `Connection::handle_datagram`; have
  `BluefinPacket::payload` be a `Bytes` slice over it (
  [`THROUGHPUT_ANALYSIS_2026.md` §3–4](../THROUGHPUT_ANALYSIS_2026.md)).
  Sans-io makes this strictly easier: there's exactly one owner of the
  buffer (the state machine), no cross-task lifetime juggling.
- **Re-enable / re-evaluate the two-hop writer.** Sans-io decouples
  the protocol decision from the runtime topology; the
  documented +8.6 % only applies if `try_send` would otherwise spin.
  Re-measure both shapes and pick.
- **`parking_lot::Mutex` for any runtime-side cross-task locks**
  ([`THROUGHPUT_ANALYSIS_2026.md` §8](../THROUGHPUT_ANALYSIS_2026.md)).
  Smaller after slice 5 because most of the contended mutexes are gone.
- **Multi-task fan-in on Linux** for `Connection::handle_datagram`.
  Today's `tx_impl` × N pattern
  ([`worker/conn_reader.rs`](../bluefin/src/worker/conn_reader.rs))
  needs care: `Connection` is `!Sync` by design (single owner). Either
  keep the existing N-recv → 1-buffer mpsc shape (each recv task
  forwards a parsed datagram to the connection task) or shard
  `Endpoint` per core. The simpler option is the former.

Acceptance gate: median bench-loopback throughput within 5 % of the
§1 baseline AND `bench_ci.sh` floors restored. If we cannot recover
the baseline after exhausting the above knobs, file an RFC issue
with measurements before accepting the regression as the new
baseline.

## 6. What this migration does NOT solve

These remain open and are explicitly out of scope:

- **Per-connection socket vs single demuxed listener** (architecture
  §10). Sans-io makes this swappable cheaply, but doesn't pick a side.
- **Multi-path / `BluefinHost::PackFollower`** (architecture §9 #4).
- **Real congestion control.** The CI throttle env-vars in
  [`bluefin/src/bin/client.rs`](../bluefin/src/bin/client.rs) stay
  in the bench binary; production CC lands in `Connection` later.
- **Retransmission of data packets.** The ack window finally has a
  natural home (`Connection`), but actually wiring "loss → retransmit"
  is its own design.

## 7. Risks and mitigations

Throughput is expected to regress during slices 3–5 and be recovered in
slice 6. `bench_ci.sh` floors will be lowered temporarily once slice 3
lands so CI stays green; the lowered floors are tracked in this doc
and restored as part of slice 6's acceptance gate.

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Throughput regression from the synchronous proto boundary on the data hot path | Expected | Lower `bench_ci.sh` floors when slice 3 lands; slice 6 is the mandatory re-tune; do not declare the migration done until slice 6 accepts |
| Floors stay lowered forever ("temporary" becomes permanent) | High if not policed | The §8 checklist requires slice 6 to restore floors; the PR description for the slice-3 floor-lowering MUST link to the slice 6 tracking issue |
| Source-compatibility break in public API | Low | Keep `BluefinClient`/`BluefinServer`/`BluefinConnection` signatures byte-identical; the adapter is the only thing that changes shape |
| Hidden assumptions in workers about packet ordering / locking that don't survive a synchronous boundary | Medium | Slices 1, 2, 3, 3b each port their tests over before deleting the old async path; never delete in the same PR that ports |
| `bluefin-io` batch-recv bugs (the documented `Ok(1)` issue) bite under sans-io | Medium | Fix in slice 4 first, behind a feature flag, with a test that exercises the multi-message path |
| Scope creep into RFC-territory questions (single-listener model, multi-path) | High | Each slice lists what it does NOT change; reviewers reject PRs that try to combine |

## 8. Acceptance checklist

Per slice (1, 2, 3, 3b, 4, 5):

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` green.
- [ ] No new `tokio::` imports under `bluefin-proto/src/**`.
- [ ] `bench_two_process.sh` runs end-to-end (functional, not perf).
- [ ] At least one new test exercises the lifted state machine in
      isolation (no Tokio runtime).
- [ ] `skills/bluefin-architecture/SKILL.md` updated to reflect the
      new boundary.
- [ ] If `bench_ci.sh` floors are lowered to accommodate the slice,
      the PR links the slice 6 tracking issue and records the
      pre-lower numbers.

Slice 6 (perf re-tune) — additionally:

- [ ] Median loopback throughput within 5 % of the pre-slice-3
      baseline recorded in §6.
- [ ] `bench_ci.sh` floors restored to (or above) their pre-migration
      values.
- [ ] Each individual knob change in §6 has its own measurement diff
      in the PR description.

---

*Created 2026-05-15. Companion to the `bluefin-architecture` and
`bluefin-protocol` skills. No code changes accompany this commit;
implementation lands one slice at a time per §5.*
