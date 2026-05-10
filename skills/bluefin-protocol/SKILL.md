---
name: bluefin-protocol
description: Wire-protocol specification for Bluefin. Bit-level header layout, packet types, connection-ID semantics, datagram packing rules, the three-way handshake state machine, packet-number rules, and acknowledgement encoding. Implementation-agnostic — the source of truth for what bytes go on the wire and what they mean. Load this whenever a task touches packet layout, framing, handshake correctness, or interoperability. **This is the seed document for the eventual Bluefin RFC.** Pair with `bluefin-architecture` for "why is the system shaped this way" and with `bluefin-101` for "where does this live in the code".
---

# Bluefin Protocol Specification (working draft)

> **Status**: pre-RFC, single-implementation. Anything marked **TBD** or **unspecified** is currently defined by the reference implementation and must be locked down before publication.

Bluefin is a connection-oriented, packet-numbered, reliable-byte-stream protocol layered directly on UDP. This skill specifies the on-the-wire format and exchange rules. It does **not** specify implementation details (those live in [bluefin-101](../bluefin-101/SKILL.md) and [bluefin-architecture](../bluefin-architecture/SKILL.md)).

## 1. Scope and non-goals

**In scope (today):**
- 20-byte fixed header.
- Five packet types: client hello, server hello, client ack, data, ack.
- Three-way handshake with random connection-ID negotiation.
- Multi-packet datagrams with same-connection / same-kind invariants.
- Cumulative ack with contiguous-range encoding.
- 64-bit packet numbers, no rollover handling specified.

**Explicitly NOT in scope yet (must be specified before RFC freeze):**
- Encryption (the `E` flag and `Mask` field exist but no encrypted packet types are defined).
- Retransmission policy.
- Congestion control.
- Flow control beyond send-side backpressure (an implementation concern today).
- Path MTU discovery.
- Connection migration / address change.
- Pack follower / multi-path operation (the [`BluefinHost::PackFollower`](../../bluefin-proto/src/context.rs) variant is reserved but unimplemented).
- 0-RTT / resumption.
- Graceful close (`FIN`/`CLOSE` packet type).

## 2. Versioning

The header carries a 4-bit `Version` field. Current value: **`0x0`**. Any peer receiving a non-`0x0` version MUST drop the packet and SHOULD log; behaviour beyond that is unspecified. Reserve `0xF` for "experimental, ignore in production".

## 3. Header layout (canonical)

Every Bluefin packet begins with a fixed 20-byte header. Network byte order (big-endian) throughout.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| Ver |  Type |       Type-specific payload      |E|    Mask     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                   Source connection ID                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                Destination connection ID                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                       Packet number                           +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Bits | Field | Encoding | Semantics |
|-----:|-------|----------|-----------|
| 4 | `Version` | `u4` | Protocol version. Currently `0x0`. |
| 4 | `Type` | `u4` | One of the values in §4. Receiver MUST drop packets with unrecognised types. |
| 16 | `Type-specific payload` | `u16` BE | Per-type meaning, see §4. |
| 1 | `E` (encrypted) | bit | Reserved for future encryption. MUST be `0` in v0. |
| 7 | `Mask` | `u7` | Header-protection mask. Reserved; MUST be `0` in v0. |
| 32 | `Source connection ID` | `u32` BE | The sender's chosen identifier for this connection. See §5. |
| 32 | `Destination connection ID` | `u32` BE | The peer's chosen identifier (or `0x0` during early handshake; see §5). |
| 64 | `Packet number` | `u64` BE | Per-direction packet counter. See §7 (data) and §8 (ack). |

The header is exactly 20 bytes. There are no optional fields, extensions, or TLVs in v0.

Reference implementation: [`bluefin/src/core/header.rs`](../../bluefin/src/core/header.rs).

## 4. Packet types

Five types are defined. The 4-bit `Type` field allows up to 16; values `0x05`–`0xF` are reserved.

| Code | Name | Has payload? | `Type-specific payload` field |
|------|------|--------------|-------------------------------|
| `0x00` | `UnencryptedClientHello` | No | Reserved; MUST be `0`. |
| `0x01` | `UnencryptedServerHello` | No | Reserved; MUST be `0`. |
| `0x02` | `ClientAck` | No | Reserved; MUST be `0`. |
| `0x03` | `UnencryptedData` | Yes | Payload length in bytes (1..=1500). |
| `0x04` | `Ack` | No | Number of contiguous packet numbers being acknowledged, starting at this packet's `Packet number` field (see §8). |

Handshake packets (`0x00`/`0x01`/`0x02`) and ack packets (`0x04`) carry **no payload bytes after the header** in v0. Data packets (`0x03`) carry exactly `Type-specific payload` bytes immediately after the header.

## 5. Connection identity

A Bluefin connection is identified by the **unordered pair** `{src_conn_id, dst_conn_id}` of two random 32-bit values, one chosen by each peer. Routing keys at each endpoint are the **directed pair** `(local_id, peer_id)` — the same connection is keyed `(A, B)` at one end and `(B, A)` at the other.

Each peer chooses its own `src_conn_id` independently and uniformly at random. There is no global registry; collisions are handled per-endpoint:

- A peer that already has an open connection with the candidate ID MUST regenerate. (The reference implementation today simply errors on collision; an RFC version should specify retry semantics.)
- A peer MUST NOT use `0x0` as a `src_conn_id`. `0x0` is reserved as the "unknown peer" sentinel during the handshake (see §6).

**Identity invariant**: every non-handshake packet MUST carry both IDs as non-zero. Receivers MUST drop packets that violate this.

## 6. Three-way handshake

The handshake consists of three packets, exchanged before any data may flow. All three are 20-byte header-only datagrams.

```
       Client                                       Server
         |                                            |
         |  1. UnencryptedClientHello                 |
         |     src_conn_id = C  (random, non-zero)    |
         |     dst_conn_id = 0x0                      |
         |     pkt_num     = Pc (random, non-zero)    |
         | -----------------------------------------> |
         |                                            |
         |  2. UnencryptedServerHello                 |
         |     src_conn_id = S  (random, non-zero)    |
         |     dst_conn_id = C                        |
         |     pkt_num     = Ps (random, non-zero)    |
         | <----------------------------------------- |
         |                                            |
         |  3. ClientAck                              |
         |     src_conn_id = C                        |
         |     dst_conn_id = S                        |
         |     pkt_num     = Pc + 1                   |
         | -----------------------------------------> |
         |                                            |
         |          ===== Connection open =====       |
         |                                            |
```

### Required validation

The receiver of each packet MUST verify:

| Step | Check |
|------|-------|
| Server on (1) | `Type == UnencryptedClientHello`, `dst_conn_id == 0x0`, `src_conn_id != 0x0`, `pkt_num != 0x0`. |
| Client on (2) | `Type == UnencryptedServerHello`, `src_conn_id != 0x0`, `dst_conn_id == C` (the client's own chosen ID), `pkt_num != 0x0`. |
| Server on (3) | `Type == ClientAck`, both IDs non-zero, `dst_conn_id == S`, `pkt_num == Pc + 1`. |

A failure on any check MUST cause the receiver to drop the packet. The reference implementation also imposes a 3 s timeout on (2) at the client and on (3) at the server — this should be a SHOULD in the RFC, with the exact value left to implementations.

### Initial packet numbers (post-handshake)

After the handshake, each side begins data with the next packet number after its hello:

- Client uses `Pc + 2` as the first data packet.
- Server uses `Ps + 1` as the first data packet.

### Open issue: hello buffering

The reference implementation has a known race: a `ClientHello` arriving before the server has called `accept()` may be dropped. The RFC SHOULD require servers to buffer up to N hellos for some bounded time, but the exact policy is **TBD**. See [bluefin-architecture §7](../bluefin-architecture/SKILL.md#7-known-architectural-debt) and live bottleneck #11 in [bluefin-performance](../bluefin-performance/SKILL.md).

## 7. Data transfer

Once the handshake completes, either side MAY send `UnencryptedData` packets at any time.

- Each direction maintains its own monotonically increasing 64-bit packet-number counter, seeded as in §6.
- Each `UnencryptedData` packet MUST carry between 1 and `MAX_BLUEFIN_PAYLOAD_SIZE_BYTES = 1500` bytes of payload. The `Type-specific payload` header field gives the exact length.
- Application bytes are a **stream**: the 1500-byte boundary has no application-visible meaning. Packets are reassembled in packet-number order at the receiver before being surfaced to the application.
- 64-bit packet numbers are large enough that wraparound is not addressed in v0. An RFC-quality version SHOULD specify either explicit wraparound rules or a connection-rotation requirement.

## 8. Datagram packing

A single UDP datagram MAY carry one or more Bluefin packets, subject to two homogeneity invariants:

1. **Same connection.** All packets in one UDP datagram MUST share the same `(src_conn_id, dst_conn_id)`.
2. **Same class.** All packets in one UDP datagram MUST be either all data (`0x03`) **or** all ack (`0x04`). Handshake packets MUST be sent one per UDP datagram.

Combined with the per-packet limit:

```
MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM = 10
MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM   = 10 × (20 + 1500) = 15200
```

These limits are constants in v0. An RFC-quality version SHOULD make them either negotiable at handshake time or deterministic functions of path MTU.

A receiver decoding a datagram parses packets sequentially: read 20-byte header → if data type, read `Type-specific payload` more bytes → advance cursor → repeat until end. A trailing partial packet is a fatal framing error and the entire datagram MUST be dropped.

## 9. Acknowledgement encoding

Acks are sent as separate datagrams (never piggy-backed on data in v0). An ack packet has:

- `Type = 0x04` (`Ack`).
- `Packet number = base` — the **first** packet number being acknowledged.
- `Type-specific payload = N` — the number of **contiguous** packet numbers being acknowledged, starting at `base`.

So an ack `(base=1000, N=200)` acknowledges packet numbers `1000, 1001, …, 1199` inclusive. Non-contiguous coverage requires multiple ack packets (which MAY be packed into the same UDP datagram per §8).

The receiver maintains a sliding window of received packet numbers and decides when to emit acks. The reference implementation triggers an ack every 200 consumed packets ([`worker/reader.rs`](../../bluefin/src/worker/reader.rs)); the RFC SHOULD specify a minimum cadence (e.g. "every M packets or every T ms, whichever first") and leave the exact values to implementations.

**Open issue: gaps.** Today only contiguous ranges are encoded. Selective-ack semantics (SACK-style) for arbitrary ranges are **TBD**.

## 10. Error handling and mismatched packets

A receiver MUST drop, without further action:

- Packets shorter than 20 bytes.
- Packets with `Version != 0x0`.
- Packets with `Type` outside the table in §4.
- Packets whose `(src_conn_id, dst_conn_id)` does not match any known connection at this endpoint, **except** for hellos as described in §6.
- `UnencryptedData` packets whose `Type-specific payload` declares more bytes than remain in the UDP datagram.
- Any packet that violates the homogeneity invariants of §8.

Receivers MUST NOT respond to dropped packets in v0. Bluefin v0 has no `RST` or `STOP` notion.

## 11. Reserved fields and forward-compatibility

| Field | Reserved value in v0 | Notes |
|-------|----------------------|-------|
| `Version` | `0x0` | `0xF` reserved for experimental. |
| `Type` codes `0x05`–`0xF` | unused | Future packet types (encrypted variants, close, ping, etc.). |
| `E` (encrypted bit) | `0` | Receivers MUST drop packets with `E=1` in v0. |
| `Mask` | `0` | Header-protection mask. Receivers MUST ignore in v0; MUST NOT enforce zero (forward-compat). |

A v1 implementation receiving a v0 packet MUST be able to interpret it as specified here. A v0 implementation receiving a v1+ packet MUST drop it (per `Version` rule above).

## 12. Open questions for the RFC

These are not bugs — they are deliberate omissions that the reference implementation has chosen not to settle. The RFC has to.

| Topic | Today | What the RFC needs |
|-------|-------|--------------------|
| Encryption | Not implemented; flag bit reserved. | Cipher suite list, key schedule, what the `Mask` byte covers, how encrypted packet types are coded. |
| Retransmission | Not implemented; receiver tracks acks but sender never retransmits. | RTO algorithm, max retransmits, fast-retransmit triggers. |
| Congestion control | None. | Slow-start? CUBIC? BBR-style? Probably "implementations MUST implement *some* CC, MUST mark it in the handshake". |
| Flow control | Send-side bounded queue (impl detail). | Receiver-advertised window, on-wire credit field. |
| Selective ack | Contiguous ranges only. | SACK encoding for non-contiguous ranges. |
| Connection close | None. | `Close`/`Goodbye` packet type with optional reason code. |
| Address migration | Not supported. | Per-connection key rotation? Address-validation token? |
| Hello buffering | Race-prone, see §6. | Required server-side hello queue depth and timeout. |
| Packet-number wrap | Undefined. | Either wraparound rules or mandatory connection rotation before exhaustion. |
| Pack follower / multi-path | `BluefinHost::PackFollower` reserved, unused. | Whether v1 includes multi-path at all, and if so the addressing model. |
| MTU and packing limits | Hardcoded constants. | Negotiated at handshake, or derived from path MTU. |

## 13. Conformance checklist (for future test vectors)

A conformant v0 implementation MUST:

- Encode/decode the 20-byte header exactly per §3.
- Reject all packets failing the validation rules of §6 and §10.
- Honour the homogeneity invariants of §8.
- Emit acks that cover only contiguous packet-number ranges per §9.
- Refuse to use `src_conn_id == 0x0` or `pkt_num == 0` outside the explicit handshake exceptions.
- Treat `E=1` packets as drop-on-receipt in v0.

A conformant v0 implementation SHOULD:

- Time out the second and third handshake steps (typical: 3 s).
- Buffer at least one in-flight `ClientHello` per pending `accept()` slot to mitigate the race in §6.
- Pack data packets up to the 15200-byte datagram limit when the send queue allows.

---

**See also**: [bluefin-architecture](../bluefin-architecture/SKILL.md) for the system-shape rationale, [bluefin-101](../bluefin-101/SKILL.md) for the code map, and [bluefin-performance](../bluefin-performance/SKILL.md) for current measured behaviour.
