use crate::core::header::BluefinHeader;
pub trait BluefinPayload {

}

pub struct BluefinPacket {
    header: BluefinHeader,
    payload: dyn BluefinPayload
}