# Bluefin Skills

Domain-specific skills for working in the Bluefin codebase. Read the relevant `SKILL.md` before doing related work.

## Available skills

| Skill | When to load |
|-------|--------------|
| [bluefin-101](bluefin-101/SKILL.md) | Any task touching Bluefin internals: connection setup, packet flow, the worker tasks, the buffer types. Load this first if you've never seen the codebase. |
| [bluefin-performance](bluefin-performance/SKILL.md) | Any task involving throughput, latency, allocations, or hot-path tuning in the worker/net/io layers. Load this *in addition to* `bluefin-101`. Consolidates the historical perf docs (now in [`../docs/archive/`](../docs/archive/)) and points at the live backlog in [`../THROUGHPUT_ANALYSIS_2026.md`](../THROUGHPUT_ANALYSIS_2026.md). |
| [bluefin-protocol](bluefin-protocol/SKILL.md) | Wire-format and on-the-wire behaviour: header layout, packet types, connection-ID rules, handshake, datagram packing invariants, ack encoding. Implementation-agnostic. **Seed document for the eventual Bluefin RFC.** Load when modifying anything that touches the bytes on the wire, or when reasoning about interoperability / forward compatibility. |
| [bluefin-architecture](bluefin-architecture/SKILL.md) | System shape and design rationale: crate layering, the dual-socket model, the connection-demux table, the buffer-with-waker pattern, task topology, threading. Companion to `bluefin-protocol` for the "how the reference implementation realises the protocol" half of the eventual RFC. Load when proposing structural changes (new task, new socket, multi-path) or when an RFC section has to make implementation-shape choices. |

## Conventions

- Each skill is a directory with a `SKILL.md` that is the entry point.
- Skills are concise and link out to specific files/lines rather than reproducing code.
- File references use workspace-relative paths so links resolve in editors.
