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
