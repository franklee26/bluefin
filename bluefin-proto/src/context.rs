/// The endpoint host type
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluefinHost {
    PackLeader,
    PackFollower,
    Client,
}
