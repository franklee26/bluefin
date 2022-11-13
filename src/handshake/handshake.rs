use crate::core::{
    context::{BluefinHost, Context},
    header::BluefinHeader,
    serialisable::Serialisable,
};

#[derive(Debug)]
pub struct BluefinHandshakeError {
    message: String,
}

impl BluefinHandshakeError {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for BluefinHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BluefinHandshakeError {}

pub fn bluefin_handshake_handle(
    context: &Context,
    packet: &[u8],
) -> Result<(), BluefinHandshakeError> {
    if context.host_type == BluefinHost::Server {
        return bluefin_server_handshake_handler(packet);
    } else {
        return bluefin_client_handshake_handler(packet);
    }
}

fn bluefin_server_handshake_handler(packet: &[u8]) -> Result<(), BluefinHandshakeError> {
    let deserialised = BluefinHeader::deserialise(packet);
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}

fn bluefin_client_handshake_handler(packet: &[u8]) -> Result<(), BluefinHandshakeError> {
    let deserialised = BluefinHeader::deserialise(packet);
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}
