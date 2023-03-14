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
    context: &mut Context,
    packet: &[u8],
) -> Result<(), BluefinHandshakeError> {
    match context.host_type {
        BluefinHost::Client => bluefin_client_handshake_handler(packet),
        BluefinHost::PackLeader => bluefin_packleader_handshake_handler(context, packet),
        BluefinHost::PackFollower => bluefin_packfollower_handshake_handler(packet),
    }
}

fn bluefin_packleader_handshake_handler(
    context: &mut Context,
    packet: &[u8],
) -> Result<(), BluefinHandshakeError> {
    let deserialised = BluefinHeader::deserialise(packet);
    eprintln!("Found header: {:?}", deserialised);

    // send something back
    // let mut buf = vec![1, 3, 1, 8];
    // let res = context.device.write(&mut buf);
    // if let Ok(write_size) = res {
    //     eprintln!("Wrote {} bytes back", write_size);
    // } else {
    //     eprintln!("Error! Failed to write back");
    // }

    Ok(())
}

fn bluefin_packfollower_handshake_handler(packet: &[u8]) -> Result<(), BluefinHandshakeError> {
    let deserialised = BluefinHeader::deserialise(packet);
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}

fn bluefin_client_handshake_handler(packet: &[u8]) -> Result<(), BluefinHandshakeError> {
    let deserialised = BluefinHeader::deserialise(packet);
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}
