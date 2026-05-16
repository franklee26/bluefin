//! Sans-io endpoint state for the Bluefin handshake.
//!
//! [`Endpoint`] owns the per-host state that drives the handshake demux:
//! the queue of `ClientHello` packets that arrived before any `accept()`
//! slot was ready, and the FIFO of `accept()` slots waiting for their
//! first hello.
//!
//! It is intentionally *pure*: no `tokio`, no sockets, no async. It
//! takes parsed [`BluefinPacket`] values plus a peer [`SocketAddr`] and
//! returns a [`HelloOutcome`] telling the runtime adapter what to do
//! next. The connection-buffer table (`DashMap<(u32, u32), …>`) and the
//! data-path routing remain in the async runtime for now — see
//! [`docs/SANS_IO_MIGRATION.md`](../../../docs/SANS_IO_MIGRATION.md)
//! slice 1b for scope.
//!
//! Replaces the dead-code [`crate::handshake::state_machine::HandshakeHandler`]
//! scaffold; that type is gone as of this slice.

use std::collections::VecDeque;
use std::net::SocketAddr;

use crate::context::BluefinHost;
use crate::wire::header::PacketType;
use crate::wire::packet::BluefinPacket;

/// Maximum number of `ClientHello` packets to buffer when no `accept()`
/// slot is ready yet. Prevents unbounded memory growth from a flood of
/// hellos. Bound is part of the reference implementation, not the
/// protocol — see [`bluefin-architecture`](../../../skills/bluefin-architecture/SKILL.md)
/// §4.
pub const MAX_QUEUED_HELLOS: usize = 64;

/// Outcome of [`Endpoint::classify_hello`] for the first packet of a
/// freshly-parsed datagram.
#[derive(Debug)]
pub enum HelloOutcome {
    /// Not a hello packet for this host type. The caller proceeds with
    /// normal data-path routing using the header connection IDs.
    NotHello,
    /// The packet is a hello and the runtime should route it to a
    /// connection keyed by `(our_id, 0)` in its connection table.
    /// - On `PackLeader` (server), `our_id` is the connection ID of the
    ///   `accept()` slot the hello was matched to.
    /// - On `Client`, `our_id` is the local ID we chose at `connect()`
    ///   time (equal to `packet.header.destination_connection_id`).
    Routed { our_id: u32 },
    /// Server-only: no `accept()` slot was ready and the hello was
    /// pushed into the internal queue for the next `accept()` call.
    /// The caller MUST drop the packet (the [`Endpoint`] owns it now).
    Queued,
    /// Server-only: the hello queue is full and the packet was
    /// silently dropped. The caller MUST drop the packet.
    Dropped,
}

/// Per-host sans-io endpoint state.
///
/// One per `BluefinClient`/`BluefinServer`. The runtime adapter is
/// expected to wrap this in an `Arc<Mutex<...>>` (or equivalent) and
/// share it with whatever reader-loop task drives the listener socket
/// + the `accept()` API.
pub struct Endpoint {
    host_type: BluefinHost,
    /// Server-side only: FIFO of `(src_conn_id)` accept slots waiting
    /// to be matched by an incoming `ClientHello`.
    pending_accept_ids: VecDeque<u32>,
    /// Server-side only: `ClientHello` packets that arrived before any
    /// matching `accept()` slot existed. Capped at [`MAX_QUEUED_HELLOS`].
    hello_queue: VecDeque<(BluefinPacket, SocketAddr)>,
}

impl Endpoint {
    pub fn new(host_type: BluefinHost) -> Self {
        Self {
            host_type,
            pending_accept_ids: VecDeque::new(),
            hello_queue: VecDeque::new(),
        }
    }

    pub fn host_type(&self) -> BluefinHost {
        self.host_type
    }

    /// Server `accept()` driver: atomically pop a queued hello if one
    /// is already waiting, otherwise register `src_conn_id` as a
    /// pending-accept slot so the next matching `ClientHello` is
    /// routed to it.
    ///
    /// Returns `Some(hello)` if a queued hello was drained (in which
    /// case the caller has everything it needs to finish the handshake
    /// without parking on the connection buffer). Returns `None` if a
    /// slot was registered and the caller must park awaiting the next
    /// hello.
    pub fn take_queued_hello_or_register(
        &mut self,
        src_conn_id: u32,
    ) -> Option<(BluefinPacket, SocketAddr)> {
        if let Some(hello) = self.hello_queue.pop_front() {
            return Some(hello);
        }
        self.pending_accept_ids.push_back(src_conn_id);
        None
    }

    /// Inspect the *first* packet of a freshly-parsed datagram (called
    /// only when the datagram contains exactly one packet, since
    /// handshake packets are always 1-per-datagram per
    /// [`bluefin-101`](../../../skills/bluefin-101/SKILL.md)). If it is
    /// a hello, consume it from `packets` (queue or route);
    /// otherwise leave `packets` untouched.
    ///
    /// `packets` is mutated rather than consumed because, in the
    /// routed case, the caller still needs to buffer the packet into
    /// the matched connection — it's left in `packets` for the
    /// caller's normal `drain(..)` loop to pick up.
    pub fn classify_hello(
        &mut self,
        packets: &mut Vec<BluefinPacket>,
        addr: SocketAddr,
    ) -> HelloOutcome {
        debug_assert_eq!(packets.len(), 1);

        if !is_hello_packet(self.host_type, &packets[0]) {
            return HelloOutcome::NotHello;
        }

        match self.host_type {
            BluefinHost::PackLeader => {
                // Take ownership before checking the slot queue so the
                // queue-push (if needed) happens atomically with the
                // slot check.
                let pkt = packets.drain(..).next().unwrap();
                if let Some(id) = self.pending_accept_ids.pop_front() {
                    // Put the packet back so the caller's drain loop
                    // routes it to the matched connection.
                    packets.push(pkt);
                    HelloOutcome::Routed { our_id: id }
                } else if self.hello_queue.len() < MAX_QUEUED_HELLOS {
                    self.hello_queue.push_back((pkt, addr));
                    HelloOutcome::Queued
                } else {
                    HelloOutcome::Dropped
                }
            }
            BluefinHost::Client => {
                // Client: ServerHello carries `(server_id, our_id)`.
                // The connection was registered under `(our_id, 0)` at
                // connect-time; route the packet there.
                let our_id = packets[0].header.destination_connection_id;
                HelloOutcome::Routed { our_id }
            }
            BluefinHost::PackFollower => unimplemented!("PackFollower handshake not designed"),
        }
    }
}

/// Returns true iff `packet` is a syntactically-valid hello packet for
/// the given host role.
///
/// - `PackLeader` expects `UnencryptedClientHello` with a non-zero
///   `source_connection_id` and a zero `destination_connection_id`.
/// - `Client` expects `UnencryptedServerHello` with both IDs non-zero.
pub fn is_hello_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    match host_type {
        BluefinHost::PackLeader => {
            packet.header.type_field == PacketType::UnencryptedClientHello
                && other_id != 0x0
                && this_id == 0x0
        }
        BluefinHost::Client => {
            packet.header.type_field == PacketType::UnencryptedServerHello
                && other_id != 0x0
                && this_id != 0x0
        }
        BluefinHost::PackFollower => false,
    }
}

/// Returns true iff `packet` is a valid `ClientAck` for the given host
/// role. Mirrors [`is_hello_packet`] for the third leg of the
/// three-way handshake.
pub fn is_client_ack_packet(host_type: BluefinHost, packet: &BluefinPacket) -> bool {
    let other_id = packet.header.source_connection_id;
    let this_id = packet.header.destination_connection_id;

    host_type == BluefinHost::PackLeader
        && packet.header.type_field == PacketType::ClientAck
        && other_id != 0x0
        && this_id != 0x0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::header::{BluefinHeader, BluefinSecurityFields};

    fn hello_packet(src: u32, dst: u32, ty: PacketType) -> BluefinPacket {
        let sec = BluefinSecurityFields::new(false, 0);
        let mut hdr = BluefinHeader::new(src, dst, ty, 0, sec);
        hdr.with_packet_number(42);
        BluefinPacket::builder().header(hdr).build()
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:1234".parse().unwrap()
    }

    #[test]
    fn is_hello_packet_server_accepts_client_hello() {
        let p = hello_packet(0xaaaa, 0, PacketType::UnencryptedClientHello);
        assert!(is_hello_packet(BluefinHost::PackLeader, &p));
    }

    #[test]
    fn is_hello_packet_server_rejects_zero_src() {
        let p = hello_packet(0, 0, PacketType::UnencryptedClientHello);
        assert!(!is_hello_packet(BluefinHost::PackLeader, &p));
    }

    #[test]
    fn is_hello_packet_client_requires_both_ids() {
        let good = hello_packet(0xbbbb, 0xcccc, PacketType::UnencryptedServerHello);
        let bad = hello_packet(0xbbbb, 0, PacketType::UnencryptedServerHello);
        assert!(is_hello_packet(BluefinHost::Client, &good));
        assert!(!is_hello_packet(BluefinHost::Client, &bad));
    }

    #[test]
    fn queued_hello_is_returned_to_accept() {
        let mut ep = Endpoint::new(BluefinHost::PackLeader);
        let mut pkts = vec![hello_packet(0xaaaa, 0, PacketType::UnencryptedClientHello)];

        // Hello arrives first, no accept slot ready.
        match ep.classify_hello(&mut pkts, addr()) {
            HelloOutcome::Queued => {}
            other => panic!("expected Queued, got {other:?}"),
        }
        assert!(pkts.is_empty(), "queued packet must leave caller's vec empty");

        // accept() comes along and drains the queue.
        let drained = ep.take_queued_hello_or_register(0x1234);
        assert!(drained.is_some());
    }

    #[test]
    fn accept_then_hello_routes_to_slot() {
        let mut ep = Endpoint::new(BluefinHost::PackLeader);

        // accept() registers a slot first.
        assert!(ep.take_queued_hello_or_register(0x1234).is_none());

        // Hello arrives second — routed to the registered slot.
        let mut pkts = vec![hello_packet(0xaaaa, 0, PacketType::UnencryptedClientHello)];
        match ep.classify_hello(&mut pkts, addr()) {
            HelloOutcome::Routed { our_id } => assert_eq!(our_id, 0x1234),
            other => panic!("expected Routed, got {other:?}"),
        }
        assert_eq!(pkts.len(), 1, "routed packet must stay in caller's vec");
    }

    #[test]
    fn hello_queue_caps_at_max_queued_hellos() {
        let mut ep = Endpoint::new(BluefinHost::PackLeader);
        // Fill the queue.
        for _ in 0..MAX_QUEUED_HELLOS {
            let mut pkts = vec![hello_packet(1, 0, PacketType::UnencryptedClientHello)];
            assert!(matches!(
                ep.classify_hello(&mut pkts, addr()),
                HelloOutcome::Queued
            ));
        }
        // One more is dropped.
        let mut pkts = vec![hello_packet(1, 0, PacketType::UnencryptedClientHello)];
        assert!(matches!(
            ep.classify_hello(&mut pkts, addr()),
            HelloOutcome::Dropped
        ));
    }

    #[test]
    fn client_classifies_server_hello_with_our_id() {
        let mut ep = Endpoint::new(BluefinHost::Client);
        let mut pkts = vec![hello_packet(0xbbbb, 0xcccc, PacketType::UnencryptedServerHello)];
        match ep.classify_hello(&mut pkts, addr()) {
            HelloOutcome::Routed { our_id } => assert_eq!(our_id, 0xcccc),
            other => panic!("expected Routed, got {other:?}"),
        }
    }

    #[test]
    fn non_hello_is_passthrough() {
        let mut ep = Endpoint::new(BluefinHost::PackLeader);
        // A data packet (not a hello).
        let sec = BluefinSecurityFields::new(false, 0);
        let hdr = BluefinHeader::new(1, 2, PacketType::UnencryptedData, 8, sec);
        let mut pkts = vec![BluefinPacket::builder().header(hdr).build()];
        match ep.classify_hello(&mut pkts, addr()) {
            HelloOutcome::NotHello => {}
            other => panic!("expected NotHello, got {other:?}"),
        }
        assert_eq!(pkts.len(), 1, "non-hello must not be consumed");
    }

    #[test]
    fn is_client_ack_requires_both_ids_and_server_role() {
        let sec = BluefinSecurityFields::new(false, 0);
        let mut hdr = BluefinHeader::new(0xaaaa, 0xbbbb, PacketType::ClientAck, 0, sec);
        hdr.with_packet_number(99);
        let p = BluefinPacket::builder().header(hdr).build();
        assert!(is_client_ack_packet(BluefinHost::PackLeader, &p));
        assert!(!is_client_ack_packet(BluefinHost::Client, &p));

        let mut hdr2 = BluefinHeader::new(0xaaaa, 0, PacketType::ClientAck, 0, sec);
        hdr2.with_packet_number(99);
        let p2 = BluefinPacket::builder().header(hdr2).build();
        assert!(!is_client_ack_packet(BluefinHost::PackLeader, &p2));
    }
}
