use crate::context::BluefinHost;
use crate::error::BluefinError;
use crate::BluefinResult;
use rand::{rng, Rng};
use std::cmp::PartialEq;
use std::collections::VecDeque;

#[derive(Eq, Debug, PartialEq)]
enum State {
    ServerStart,
    PendingAccept,
    RecvClientHello,
    SentServerHello,
    RecvClientAck,
    ClientStart,
    RequestingAccept,
    SentClientHello,
    RecvServerHello,
    SentClientAck,
    Conn,
}

struct PendingAccept {
    src_conn_id: u32,
    packet_number: u64,
}

impl PendingAccept {
    pub fn new() -> PendingAccept {
        Self {
            src_conn_id: rng().random(),
            packet_number: rng().random(),
        }
    }
}

pub struct Transmit {
    src_conn_id: u32,
    packet_number: u64,
}

impl Transmit {
    pub fn new() -> Transmit {
        Self {
            src_conn_id: rng().random(),
            packet_number: rng().random(),
        }
    }
}

pub struct HandshakeHandler {
    state: State,
    pending_accepts: VecDeque<PendingAccept>,
    transmit_queue: VecDeque<Transmit>,
}

impl HandshakeHandler {
    pub fn new(host_type: BluefinHost) -> HandshakeHandler {
        let pending_accepts = VecDeque::new();
        let transmit_queue = VecDeque::new();
        match host_type {
            BluefinHost::Client => Self {
                state: State::ClientStart,
                pending_accepts,
                transmit_queue,
            },
            BluefinHost::PackLeader => Self {
                state: State::ServerStart,
                pending_accepts,
                transmit_queue,
            },
            _ => todo!(),
        }
    }

    /// This initiates the handshake. For the pack leader we enqueue a pending accept request,
    /// which indicates that the server is ready to accept a connection. Each pending accept request
    /// represents one potential Bluefin connection. For the client we enqueue a transmit request
    /// which represents a client-hello packet to be sent on the wire.
    pub fn begin(&mut self) -> BluefinResult<()> {
        match &self.state {
            // Push a pending accept, transition into PendingAccept state and wait
            State::ServerStart => {
                self.pending_accepts.push_back(PendingAccept::new());
                self.state = State::PendingAccept;
                Ok(())
            }
            // Push an accept request into the transmit queue, transition into RequestingAccept
            // state and wait.
            State::ClientStart => {
                self.transmit_queue.push_back(Transmit::new());
                self.state = State::RequestingAccept;
                Ok(())
            }
            // It is not valid to call this function and be in any state other than ServerStart
            // or ClientStart. Fail here.
            other => Err(BluefinError::InvalidState(format!(
                "Cannot begin handshake at state: {:?}",
                other
            ))),
        }
    }

    pub fn poll_tx_client_hello(&mut self) -> BluefinResult<Option<Transmit>> {
        if self.state != State::RequestingAccept {
            return Err(BluefinError::InvalidState(
                "Cannot poll tx client hello".to_string(),
            ));
        }
        Ok(self.transmit_queue.pop_front())
    }

    pub fn handle(&mut self) -> BluefinResult<()> {
        Ok(())
    }
}
