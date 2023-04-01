/// The endpoint host type
#[derive(Debug, PartialEq, Eq)]
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
pub struct Context {
    pub host_type: BluefinHost,
    pub state: State,
    pub packet_number: i64,
}
