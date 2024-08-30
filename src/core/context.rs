/// The endpoint host type
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluefinHost {
    PackLeader,
    PackFollower,
    Client,
}

/// The state at which the connection is at
#[derive(PartialEq, Debug)]
pub enum State {
    Handshake,
    DataStream,
    Closed,
    Error,
    Ready,
}

#[derive(Debug)]
pub(crate) struct Context {
    pub host_type: BluefinHost,
    pub state: State,
    pub next_recv_packet_number: u64,
    pub next_send_packet_number: u64,
}
