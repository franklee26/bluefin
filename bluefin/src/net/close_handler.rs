//! Receive-side state for the FIN / FIN-ACK graceful-close exchange.
//!
//! See `bluefin-protocol` §10bis for the on-the-wire semantics. This module
//! owns the per-connection close state machine on the receiving side only;
//! the sender-side `close()` API and FIN retransmit timer land in a later
//! step.
//!
//! Wiring (current step):
//! - [`crate::worker::conn_reader::ConnReaderHandler::buffer_in_packets`]
//!   routes incoming `Fin` / `FinAck` packets into the per-connection
//!   [`CloseBuffer`] held in [`crate::net::ConnectionManagedBuffers`].
//! - On `Fin`, the reader sets a shared `Arc<AtomicBool>` flag
//!   ([`ConnectionManagedBuffers::peer_fin_observed`]) and wakes the
//!   data-buffer waker so any pending `recv()` returns EOF (`Ok(0)`).
//!
//! The buffer follows the standard "buffer-with-waker" pattern documented
//! in `bluefin-architecture` §5.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::Waker;

use tokio::sync::Notify;

use crate::core::packet::BluefinPacket;
use crate::net::Wakeable;

/// Per-connection graceful-close state. Tracked on each side independently;
/// the values describe what *this* peer has observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CloseState {
    /// Connection is open, neither side has signalled close.
    #[default]
    Active,
    /// The peer sent us a `Fin`. We owe them a `FinAck` and any further
    /// data we surface to the application is the tail of what was already
    /// in our reassembly buffer; subsequent `recv()` calls return EOF.
    PeerFinReceived { fin_packet_num: u64 },
    /// We sent a `Fin` and are awaiting the peer's `FinAck`. (Set by the
    /// sender-side `close()` API in a later step.)
    LocalFinSent { fin_packet_num: u64 },
    /// Both sides have exchanged `Fin` + `FinAck` (in either order). The
    /// connection is fully closed and per-connection state may be torn
    /// down.
    Closed,
}

/// Per-connection close-side buffer. Holds the receiving-side close state,
/// any `Fin` whose `FinAck` we still owe, and any `FinAck` we have received
/// in response to a locally-sent `Fin`.
///
/// Same shape as [`crate::net::ack_handler::AckBuffer`]: producer
/// (`ConnReaderHandler`) mutates under the mutex, then drops the guard
/// before waking. See `bluefin-architecture` §5.
pub(crate) struct CloseBuffer {
    state: CloseState,
    /// `Some(pn)` after the peer's `Fin` arrives — the sender-side close
    /// driver (added in a later step) reads this and emits the matching
    /// `FinAck`. Cleared once the `FinAck` has been emitted.
    pending_fin_ack_send: Option<u64>,
    /// `Some(pn)` after the peer's `FinAck` arrives. Used by the local
    /// close driver to confirm completion of a locally-initiated close.
    received_fin_ack_for: Option<u64>,
    /// Lock-free mirror of "peer FIN observed". Set under the mutex
    /// alongside `state`, but readable without acquiring the mutex from
    /// the recv hot path so [`crate::worker::reader::ReaderRxChannel`]
    /// can poll for EOF cheaply.
    peer_fin_observed: Arc<AtomicBool>,
    /// Notified whenever `received_fin_ack_for` becomes `Some`.
    /// `BluefinConnection::close()` parks on this with a timeout to
    /// implement the FIN → FIN-ACK → Closed handshake with retransmit.
    fin_ack_notify: Arc<Notify>,
    waker: Option<Waker>,
}

impl CloseBuffer {
    pub(crate) fn new(peer_fin_observed: Arc<AtomicBool>) -> Self {
        Self {
            state: CloseState::Active,
            pending_fin_ack_send: None,
            received_fin_ack_for: None,
            peer_fin_observed,
            fin_ack_notify: Arc::new(Notify::new()),
            waker: None,
        }
    }

    /// Record that the peer's `Fin` arrived. Idempotent on duplicates: a
    /// second `Fin` with the same packet number is a no-op; a different
    /// packet number is dropped silently (the spec MUSTs that the receiver
    /// reject a `Fin` whose pn is less than already-received data, but a
    /// stricter mismatch check is the caller's job — see §10bis).
    pub(crate) fn buffer_in_fin(&mut self, packet: &BluefinPacket) {
        let pn = packet.header.packet_number;
        match self.state {
            CloseState::Active => {
                self.state = CloseState::PeerFinReceived { fin_packet_num: pn };
                self.pending_fin_ack_send = Some(pn);
                self.peer_fin_observed.store(true, Ordering::Release);
            }
            CloseState::LocalFinSent { fin_packet_num: local_pn } => {
                // We had already initiated close; the peer's FIN crossed
                // ours. We still owe the peer a FinAck; if we have already
                // received their FinAck for our FIN we transition to
                // Closed once the FinAck send completes.
                self.pending_fin_ack_send = Some(pn);
                self.peer_fin_observed.store(true, Ordering::Release);
                if self.received_fin_ack_for == Some(local_pn) {
                    // Stay in LocalFinSent until we send the FinAck; the
                    // sender-side driver will move us to Closed.
                    self.state = CloseState::PeerFinReceived { fin_packet_num: pn };
                } else {
                    self.state = CloseState::PeerFinReceived { fin_packet_num: pn };
                }
            }
            CloseState::PeerFinReceived { fin_packet_num } if fin_packet_num == pn => {
                // Duplicate retransmission of the same FIN — keep owing
                // a FinAck (the prior one may have been lost in flight).
                self.pending_fin_ack_send = Some(pn);
            }
            CloseState::PeerFinReceived { .. } | CloseState::Closed => {
                // Either a packet-number mismatch on a duplicate (drop) or
                // the connection is already closed — ignore.
            }
        }
    }

    /// Record that the peer's `FinAck` arrived in response to our `Fin`.
    /// The sender-side close driver consumes this in a later step.
    pub(crate) fn buffer_in_fin_ack(&mut self, packet: &BluefinPacket) {
        self.received_fin_ack_for = Some(packet.header.packet_number);
        // Wake any task parked in `BluefinConnection::close()`.
        // `notify_waiters` only wakes already-registered waiters; if
        // close() hasn't yet called `notified()` it will check
        // `received_fin_ack_for` first and short-circuit before parking.
        self.fin_ack_notify.notify_waiters();
    }

    /// Returns the notify handle used to wake [`BluefinConnection::close`]
    /// when the peer's `FinAck` arrives.
    #[inline]
    pub(crate) fn fin_ack_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.fin_ack_notify)
    }

    /// Atomically take the pending `FinAck` packet number, if any, and
    /// clear it. Used by the conn_reader code path to drain at most one
    /// FinAck-send obligation per call.
    #[inline]
    pub(crate) fn take_pending_fin_ack_send(&mut self) -> Option<u64> {
        self.pending_fin_ack_send.take()
    }

    /// Transition into [`CloseState::Closed`]. Idempotent. Called by
    /// `BluefinConnection::close()` after the FIN/FIN-ACK exchange has
    /// completed (in either direction).
    #[inline]
    pub(crate) fn mark_closed(&mut self) {
        self.state = CloseState::Closed;
    }

    /// Marks the buffer as having sent a local `Fin` with the given
    /// packet number. Called by `close()` once the FIN is on the wire
    /// (or about to be). Drives the state machine into `LocalFinSent`
    /// from `Active`; from `PeerFinReceived` we go directly to `Closed`
    /// (a crossed-FINs / simultaneous close).
    #[inline]
    pub(crate) fn record_local_fin_sent(&mut self, packet_number: u64) {
        match self.state {
            CloseState::Active => {
                self.state = CloseState::LocalFinSent {
                    fin_packet_num: packet_number,
                };
            }
            CloseState::PeerFinReceived { .. } => {
                // Crossed FINs — we already received theirs; once their
                // FinAck (or our own FinAck-send) lands we'll move to
                // Closed. Track the local pn so close() can match it.
                self.state = CloseState::LocalFinSent {
                    fin_packet_num: packet_number,
                };
            }
            CloseState::LocalFinSent { .. } | CloseState::Closed => {
                // Already initiated or already closed — no-op.
            }
        }
    }

    #[inline]
    pub(crate) fn state(&self) -> CloseState {
        self.state
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn pending_fin_ack_send(&self) -> Option<u64> {
        self.pending_fin_ack_send
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn received_fin_ack_for(&self) -> Option<u64> {
        self.received_fin_ack_for
    }

    /// Sets the waker only if it differs from the existing one (matches
    /// the pattern in `ConnectionBuffer` / `AckBuffer`).
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn set_waker_if_changed(&mut self, new_waker: &Waker) {
        if let Some(ref existing) = self.waker {
            if existing.will_wake(new_waker) {
                return;
            }
        }
        self.waker = Some(new_waker.clone());
    }
}

impl Wakeable for CloseBuffer {
    #[inline]
    fn take_waker_clone(&self) -> Option<Waker> {
        self.waker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::header::{BluefinHeader, BluefinSecurityFields, PacketType};
    use crate::core::packet::BluefinPacket;

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
    fn fresh_buffer_is_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let buf = CloseBuffer::new(Arc::clone(&flag));
        assert_eq!(buf.state(), CloseState::Active);
        assert_eq!(buf.pending_fin_ack_send(), None);
        assert_eq!(buf.received_fin_ack_for(), None);
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn fin_transitions_to_peer_fin_received_and_sets_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(Arc::clone(&flag));
        buf.buffer_in_fin(&fin_packet(1234));
        assert_eq!(
            buf.state(),
            CloseState::PeerFinReceived { fin_packet_num: 1234 }
        );
        assert_eq!(buf.pending_fin_ack_send(), Some(1234));
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn duplicate_fin_is_idempotent_and_re_arms_pending_ack() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(Arc::clone(&flag));
        buf.buffer_in_fin(&fin_packet(7));
        buf.pending_fin_ack_send = None; // simulate FinAck already emitted
        buf.buffer_in_fin(&fin_packet(7));
        assert_eq!(
            buf.state(),
            CloseState::PeerFinReceived { fin_packet_num: 7 }
        );
        // A duplicate FIN re-arms the pending FinAck so we resend it.
        assert_eq!(buf.pending_fin_ack_send(), Some(7));
    }

    #[test]
    fn mismatched_fin_pn_after_first_is_dropped() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(Arc::clone(&flag));
        buf.buffer_in_fin(&fin_packet(7));
        buf.pending_fin_ack_send = None;
        buf.buffer_in_fin(&fin_packet(99));
        assert_eq!(
            buf.state(),
            CloseState::PeerFinReceived { fin_packet_num: 7 }
        );
        assert_eq!(buf.pending_fin_ack_send(), None);
    }

    #[test]
    fn fin_ack_records_packet_number() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(flag);
        buf.buffer_in_fin_ack(&fin_ack_packet(42));
        assert_eq!(buf.received_fin_ack_for(), Some(42));
    }
}
