//! Per-connection sans-io state machines.
//!
//! Today this is just the close (FIN / FIN-ACK) FSM; later slices of the
//! sans-io migration (see [`docs/SANS_IO_MIGRATION.md`](../../../docs/SANS_IO_MIGRATION.md))
//! grow this into the full per-connection state machine: ack window,
//! ordered reassembly, packetisation, retransmit timing.
//!
//! Nothing here may depend on `tokio` or any I/O primitive — the
//! `no_io_deps` guardrail enforces that at crate level.

pub mod close;
