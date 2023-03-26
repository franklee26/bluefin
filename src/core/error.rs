use std::error;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BluefinError {
    #[error("Unable to serialise data")]
    SerialiseError,

    #[error("`{0}`")]
    DeserialiseError(String),

    #[error("Unable to complete handshake: `{0}`")]
    HandshakeError(String),

    #[error("`{0}`")]
    InvalidHeaderError(String),

    #[error("`Payload size of {0} bytes is too large`")]
    LargePayloadError(String),

    #[error("Encountered error while reading from socket: `{0}`")]
    ReadError(String),
}
