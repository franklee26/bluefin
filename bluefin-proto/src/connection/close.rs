//! Sans-io close state machine for the FIN / FIN-ACK exchange.
//!
//! See `bluefin-protocol` §10bis for the on-the-wire semantics. This
//! module owns the pure FSM: it accepts inbound `Fin`/`FinAck` packets
//! and local `close()` calls, and exposes a single "do you owe the peer
//! a `FinAck`?" obligation slot for the runtime to drain.
//!
//! Runtime concerns (the `Notify` that wakes a parked `close().await`,
//! the lock-free `AtomicBool` that the recv hot path polls for EOF, the
//! `Waker` for cross-task wake-after-buffer-in) deliberately stay in the
//! `bluefin::net::close_handler::CloseBuffer` adapter that wraps this
//! type. The boundary is the sans-io seam from
//! [`docs/SANS_IO_MIGRATION.md`](../../../../docs/SANS_IO_MIGRATION.md)
//! §5 slice 2.
//!
//! Lifted from `bluefin::net::close_handler::CloseBuffer` (the wrapper
//! retains the same tests on the runtime layer for the
//! `AtomicBool`/`Notify` plumbing).

use crate::wire::packet::BluefinPacket;

/// Per-connection graceful-close state. Tracked on each side
/// independently; the values describe what *this* peer has observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseState {
    /// Connection is open, neither side has signalled close.
    #[default]
    Active,
    /// The peer sent us a `Fin`. We owe them a `FinAck`; any further
    /// data we surface to the application is the tail of what was
    /// already in our reassembly buffer; subsequent `recv()` calls
    /// return EOF.
    PeerFinReceived { fin_packet_num: u64 },
    /// We sent a `Fin` and are awaiting the peer's `FinAck`.
    LocalFinSent { fin_packet_num: u64 },
    /// Both sides have exchanged `Fin` + `FinAck` (in either order).
    /// Per-connection state may be torn down.
    Closed,
}

/// Pure close-side state machine. No async, no sockets, no wakers.
///
/// The runtime adapter (`bluefin::net::close_handler::CloseBuffer`)
/// wraps this and adds the cross-task wake plumbing.
#[derive(Debug, Default)]
pub struct CloseFsm {
    state: CloseState,
    /// `Some(pn)` after the peer's `Fin` arrives — the sender-side
    /// close driver reads this via [`Self::take_pending_fin_ack_send`]
    /// and emits the matching `FinAck`.
    pending_fin_ack_send: Option<u64>,
    /// `Some(pn)` after the peer's `FinAck` arrives. Used by the local
    /// close driver to confirm completion of a locally-initiated close.
    received_fin_ack_for: Option<u64>,
}

/// Side-effects the runtime adapter must perform after a state-machine
/// transition. The FSM never touches `Waker`s, `Notify`s, or atomics
/// itself; instead it returns one of these so the adapter can fire the
/// appropriate cross-task wake outside the FSM's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseEvent {
    /// No externally-visible state change. Adapter does nothing.
    None,
    /// The peer's `Fin` was just observed. Adapter should: set its
    /// `peer_fin_observed` flag, wake the recv-side data buffer's
    /// waker (so a parked `recv()` returns EOF).
    PeerFinObserved { fin_packet_num: u64 },
    /// The peer's `FinAck` was just observed. Adapter should notify
    /// the task parked inside `close().await`.
    PeerFinAckObserved { fin_packet_num: u64 },
}

impl CloseFsm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the peer's `Fin` arrived. Idempotent on duplicates:
    /// a second `Fin` with the same packet number re-arms the pending
    /// `FinAck`-send obligation (so we resend it if the prior one was
    /// lost); a different packet number after the first is dropped
    /// silently per §10bis.
    ///
    /// Returns a [`CloseEvent`] describing what cross-task wake (if
    /// any) the runtime adapter must perform.
    pub fn buffer_in_fin(&mut self, packet: &BluefinPacket) -> CloseEvent {
        let pn = packet.header.packet_number;
        match self.state {
            CloseState::Active => {
                self.state = CloseState::PeerFinReceived { fin_packet_num: pn };
                self.pending_fin_ack_send = Some(pn);
                CloseEvent::PeerFinObserved { fin_packet_num: pn }
            }
            CloseState::LocalFinSent { fin_packet_num: _local_pn } => {
                // We had already initiated close; the peer's FIN crossed
                // ours. We still owe the peer a FinAck. Transition to
                // PeerFinReceived; the local-fin handshake continues in
                // parallel via received_fin_ack_for.
                self.pending_fin_ack_send = Some(pn);
                self.state = CloseState::PeerFinReceived { fin_packet_num: pn };
                CloseEvent::PeerFinObserved { fin_packet_num: pn }
            }
            CloseState::PeerFinReceived { fin_packet_num } if fin_packet_num == pn => {
                // Duplicate retransmission of the same FIN — re-arm the
                // FinAck-send obligation (prior may have been lost) but
                // do not re-fire the observer wake.
                self.pending_fin_ack_send = Some(pn);
                CloseEvent::None
            }
            CloseState::PeerFinReceived { .. } | CloseState::Closed => {
                // Packet-number mismatch on a duplicate (drop) or
                // already closed — ignore.
                CloseEvent::None
            }
        }
    }

    /// Record that the peer's `FinAck` arrived in response to our `Fin`.
    /// Returns a [`CloseEvent`] so the adapter can notify any task
    /// parked in `close().await`.
    pub fn buffer_in_fin_ack(&mut self, packet: &BluefinPacket) -> CloseEvent {
        let pn = packet.header.packet_number;
        self.received_fin_ack_for = Some(pn);
        CloseEvent::PeerFinAckObserved { fin_packet_num: pn }
    }

    /// Atomically take the pending `FinAck` packet number, if any, and
    /// clear it. Used by the conn_reader code path to drain at most one
    /// FinAck-send obligation per call.
    #[inline]
    pub fn take_pending_fin_ack_send(&mut self) -> Option<u64> {
        self.pending_fin_ack_send.take()
    }

    /// Transition into [`CloseState::Closed`]. Idempotent. Called by
    /// the runtime `close()` driver after the FIN/FIN-ACK exchange has
    /// completed in either direction.
    #[inline]
    pub fn mark_closed(&mut self) {
        self.state = CloseState::Closed;
    }

    /// Marks the FSM as having sent a local `Fin` with the given packet
    /// number. From `Active` we transition to `LocalFinSent`; from
    /// `PeerFinReceived` (crossed FINs) we also record the local pn so
    /// `close()` can match it against the eventual `FinAck`. Idempotent
    /// from `LocalFinSent`/`Closed`.
    #[inline]
    pub fn record_local_fin_sent(&mut self, packet_number: u64) {
        match self.state {
            CloseState::Active | CloseState::PeerFinReceived { .. } => {
                self.state = CloseState::LocalFinSent {
                    fin_packet_num: packet_number,
                };
            }
            CloseState::LocalFinSent { .. } | CloseState::Closed => {}
        }
    }

    #[inline]
    pub fn state(&self) -> CloseState {
        self.state
    }

    #[inline]
    pub fn pending_fin_ack_send(&self) -> Option<u64> {
        self.pending_fin_ack_send
    }

    #[inline]
    pub fn received_fin_ack_for(&self) -> Option<u64> {
        self.received_fin_ack_for
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::header::{BluefinHeader, BluefinSecurityFields, PacketType};

    fn fin_packet(pn: u64) -> BluefinPacket {
        let mut header = BluefinHeader::new(
            0x1111_2222,
            0x3333_4444,
            PacketType::Fin,
            0x0,
            BluefinSecurityFields::default(),
        );
        header.with_packet_number(pn);
        BluefinPacket::builder().header(header).build()
    }

    fn fin_ack_packet(pn: u64) -> BluefinPacket {
        let mut header = BluefinHeader::new(
            0x1111_2222,
            0x3333_4444,
            PacketType::FinAck,
            0x0,
            BluefinSecurityFields::default(),
        );
        header.with_packet_number(pn);
        BluefinPacket::builder().header(header).build()
    }

    #[test]
    fn fresh_fsm_is_active() {
        let fsm = CloseFsm::new();
        assert_eq!(fsm.state(), CloseState::Active);
        assert_eq!(fsm.pending_fin_ack_send(), None);
        assert_eq!(fsm.received_fin_ack_for(), None);
    }

    #[test]
    fn fin_transitions_to_peer_fin_received_and_emits_event() {
        let mut fsm = CloseFsm::new();
        let ev = fsm.buffer_in_fin(&fin_packet(1234));
        assert_eq!(ev, CloseEvent::PeerFinObserved { fin_packet_num: 1234 });
        assert_eq!(
            fsm.state(),
            CloseState::PeerFinReceived { fin_packet_num: 1234 }
        );
        assert_eq!(fsm.pending_fin_ack_send(), Some(1234));
    }

    #[test]
    fn duplicate_fin_re_arms_ack_but_does_not_re_emit_event() {
        let mut fsm = CloseFsm::new();
        assert!(matches!(
            fsm.buffer_in_fin(&fin_packet(7)),
            CloseEvent::PeerFinObserved { .. }
        ));
        let _ = fsm.take_pending_fin_ack_send(); // simulate FinAck emitted
        let ev = fsm.buffer_in_fin(&fin_packet(7));
        assert_eq!(ev, CloseEvent::None);
        assert_eq!(fsm.pending_fin_ack_send(), Some(7));
    }

    #[test]
    fn mismatched_fin_pn_after_first_is_dropped() {
        let mut fsm = CloseFsm::new();
        fsm.buffer_in_fin(&fin_packet(7));
        let _ = fsm.take_pending_fin_ack_send();
        let ev = fsm.buffer_in_fin(&fin_packet(99));
        assert_eq!(ev, CloseEvent::None);
        assert_eq!(
            fsm.state(),
            CloseState::PeerFinReceived { fin_packet_num: 7 }
        );
        assert_eq!(fsm.pending_fin_ack_send(), None);
    }

    #[test]
    fn fin_ack_records_packet_number_and_emits_event() {
        let mut fsm = CloseFsm::new();
        let ev = fsm.buffer_in_fin_ack(&fin_ack_packet(42));
        assert_eq!(ev, CloseEvent::PeerFinAckObserved { fin_packet_num: 42 });
        assert_eq!(fsm.received_fin_ack_for(), Some(42));
    }

    #[test]
    fn record_local_fin_then_peer_fin_keeps_pn() {
        // crossed-FIN: local close() sets LocalFinSent, then peer FIN
        // arrives and transitions to PeerFinReceived. The local pn
        // remains tracked via received_fin_ack_for once the FinAck
        // lands.
        let mut fsm = CloseFsm::new();
        fsm.record_local_fin_sent(500);
        assert_eq!(
            fsm.state(),
            CloseState::LocalFinSent { fin_packet_num: 500 }
        );
        let ev = fsm.buffer_in_fin(&fin_packet(501));
        assert!(matches!(ev, CloseEvent::PeerFinObserved { .. }));
        assert_eq!(
            fsm.state(),
            CloseState::PeerFinReceived { fin_packet_num: 501 }
        );

        // Now the peer's FinAck for our 500 arrives.
        let ev = fsm.buffer_in_fin_ack(&fin_ack_packet(500));
        assert_eq!(ev, CloseEvent::PeerFinAckObserved { fin_packet_num: 500 });
        assert_eq!(fsm.received_fin_ack_for(), Some(500));
    }

    #[test]
    fn mark_closed_is_idempotent_and_blocks_further_local_fin() {
        let mut fsm = CloseFsm::new();
        fsm.mark_closed();
        assert_eq!(fsm.state(), CloseState::Closed);
        fsm.record_local_fin_sent(123);
        assert_eq!(fsm.state(), CloseState::Closed);
        fsm.mark_closed();
        assert_eq!(fsm.state(), CloseState::Closed);
    }
}
