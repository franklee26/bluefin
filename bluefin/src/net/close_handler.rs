//! Runtime adapter around the sans-io close FSM.
//!
//! The pure state machine (FIN / FIN-ACK transitions, pending-FinAck
//! obligation slot, received-FinAck record) lives in
//! [`bluefin_proto::connection::close::CloseFsm`]. This module adds the
//! cross-task wake plumbing that the proto layer is forbidden from
//! touching: the `Notify` that wakes a task parked inside
//! `BluefinConnection::close().await`, the lock-free `AtomicBool` that
//! the recv hot path polls for EOF without taking the close mutex, and
//! the `Option<Waker>` that pages the recv-data buffer when the peer
//! sends `Fin`.
//!
//! Wiring:
//! - [`crate::worker::conn_reader::ConnReaderHandler::buffer_in_packets`]
//!   routes incoming `Fin` / `FinAck` packets into the per-connection
//!   [`CloseBuffer`] held in [`crate::net::ConnectionManagedBuffers`].
//! - The FSM returns a [`CloseEvent`] describing what cross-task wake
//!   to perform; the adapter performs it outside the FSM's lock.
//!
//! See `bluefin-architecture` §5 (buffer-with-waker pattern) and
//! `docs/SANS_IO_MIGRATION.md` §5 slice 2.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::Waker;

use bluefin_proto::connection::close::{CloseEvent, CloseFsm};
pub(crate) use bluefin_proto::connection::close::CloseState;
use tokio::sync::Notify;

use crate::core::packet::BluefinPacket;
use crate::net::Wakeable;

/// Per-connection close-side buffer. Runtime wrapper over [`CloseFsm`].
pub(crate) struct CloseBuffer {
    fsm: CloseFsm,
    /// Lock-free mirror of "peer FIN observed". Set by the adapter
    /// whenever the FSM emits [`CloseEvent::PeerFinObserved`]. Read by
    /// the recv hot path in [`crate::worker::reader::ReaderRxChannel`]
    /// without acquiring the close-buffer mutex.
    peer_fin_observed: Arc<AtomicBool>,
    /// Notified whenever the FSM emits [`CloseEvent::PeerFinAckObserved`].
    /// `BluefinConnection::close()` parks on this with a timeout to
    /// implement the FIN -> FIN-ACK -> Closed handshake with retransmit.
    fin_ack_notify: Arc<Notify>,
    waker: Option<Waker>,
}

impl CloseBuffer {
    pub(crate) fn new(peer_fin_observed: Arc<AtomicBool>) -> Self {
        Self {
            fsm: CloseFsm::new(),
            peer_fin_observed,
            fin_ack_notify: Arc::new(Notify::new()),
            waker: None,
        }
    }

    /// Record that the peer's `Fin` arrived. Applies the [`CloseEvent`]
    /// returned by the FSM (sets `peer_fin_observed` if applicable).
    /// Does NOT fire the data-buffer `Waker` here -- the conn_reader
    /// path owns that, so it can wake-after-drop following the
    /// architecture §5 contract.
    pub(crate) fn buffer_in_fin(&mut self, packet: &BluefinPacket) {
        let event = self.fsm.buffer_in_fin(packet);
        self.apply_event(event);
    }

    /// Record that the peer's `FinAck` arrived. Applies the
    /// [`CloseEvent`] returned by the FSM (notifies the close-driver).
    pub(crate) fn buffer_in_fin_ack(&mut self, packet: &BluefinPacket) {
        let event = self.fsm.buffer_in_fin_ack(packet);
        self.apply_event(event);
    }

    #[inline]
    fn apply_event(&mut self, event: CloseEvent) {
        match event {
            CloseEvent::None => {}
            CloseEvent::PeerFinObserved { .. } => {
                self.peer_fin_observed.store(true, Ordering::Release);
            }
            CloseEvent::PeerFinAckObserved { .. } => {
                // `notify_waiters` only wakes already-registered waiters;
                // if `close()` hasn't yet called `notified()` it will check
                // `received_fin_ack_for` first and short-circuit before
                // parking.
                self.fin_ack_notify.notify_waiters();
            }
        }
    }

    /// Returns the notify handle used to wake `BluefinConnection::close`
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
        self.fsm.take_pending_fin_ack_send()
    }

    /// Transition into [`CloseState::Closed`]. Idempotent.
    #[inline]
    pub(crate) fn mark_closed(&mut self) {
        self.fsm.mark_closed();
    }

    /// Marks the buffer as having sent a local `Fin` with the given
    /// packet number.
    #[inline]
    pub(crate) fn record_local_fin_sent(&mut self, packet_number: u64) {
        self.fsm.record_local_fin_sent(packet_number);
    }

    #[inline]
    pub(crate) fn state(&self) -> CloseState {
        self.fsm.state()
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn pending_fin_ack_send(&self) -> Option<u64> {
        self.fsm.pending_fin_ack_send()
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn received_fin_ack_for(&self) -> Option<u64> {
        self.fsm.received_fin_ack_for()
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
    //! Wrapper-level tests: the pure state-machine semantics are tested
    //! in `bluefin_proto::connection::close::tests`; here we just
    //! verify that the adapter correctly applies the `CloseEvent`
    //! to the `AtomicBool` / `Notify` runtime surfaces.

    use super::*;
    use crate::core::header::{BluefinHeader, BluefinSecurityFields, PacketType};

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
    fn buffer_in_fin_sets_peer_fin_observed_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(Arc::clone(&flag));
        assert!(!flag.load(Ordering::Acquire));
        buf.buffer_in_fin(&fin_packet(1));
        assert!(flag.load(Ordering::Acquire));
        assert_eq!(buf.state(), CloseState::PeerFinReceived { fin_packet_num: 1 });
        assert_eq!(buf.pending_fin_ack_send(), Some(1));
    }

    #[tokio::test]
    async fn buffer_in_fin_ack_wakes_notify_waiter() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut buf = CloseBuffer::new(flag);
        let notify = buf.fin_ack_notify();

        // Register a waiter, then deliver the FinAck.
        let waiter = tokio::spawn(async move { notify.notified().await });
        // Yield so the waiter actually parks before we fire.
        tokio::task::yield_now().await;
        buf.buffer_in_fin_ack(&fin_ack_packet(7));
        waiter.await.unwrap();
        assert_eq!(buf.received_fin_ack_for(), Some(7));
    }
}
