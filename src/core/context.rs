/// The endpoint host type
#[derive(Debug, PartialEq, Eq)]
pub enum BluefinHost {
    PackLeader,
    PackFollower,
    Client,
}

/// The state at which the connection is at
pub enum State {
    Handshake,
    DataStream,
    Closed,
    Error,
}

pub struct Context {
    pub host_type: BluefinHost,
    pub state: State,
}
