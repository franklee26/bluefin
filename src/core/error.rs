use thiserror::Error;

#[derive(Error, Debug)]
pub enum BluefinError {
    #[error("Unable to serialise data")]
    SerialiseError,

    #[error("`{0}`")]
    DeserialiseError(String),

    #[error("Connection buffer does not exist")]
    BufferDoesNotExist,

    #[error("Current buffer is full.")]
    BufferFullError,

    #[error("Current buffer is empty.")]
    BufferEmptyError,

    #[error("Unable to accept new connection: `{0}`")]
    CouldNotAcceptConnectionError(String),

    #[error("Unable to complete handshake: `{0}`")]
    HandshakeError(String),

    #[error("`{0}`")]
    InvalidHeaderError(String),

    #[error("`Payload size of {0} bytes is too large`")]
    LargePayloadError(String),

    #[error("Encountered error while reading from socket: `{0}`")]
    ReadError(String),

    #[error("Encountered error while writing to socket: `{0}`")]
    WriteError(String),

    #[error("Cannot currently open stream for given connection")]
    CannotOpenStreamError,

    #[error("Cannot currently accept new connection due to too many connections opened")]
    TooManyOpenConnectionsError,

    #[error("No such connection found")]
    NoSuchConnectionError,

    #[error("Connection already exists. Nothing done.")]
    ConnectionAlreadyExists,

    #[error("Stream already exists. Nothing done.")]
    StreamAlreadyExists,

    #[error("No such waker.")]
    NoSuchWakerError,

    #[error("Socket is not valid.")]
    InvalidSocketError,

    #[error("Encountered segment with unexpected segment number.")]
    UnexpectedSegmentError,

    #[error("Could not buffer data: `{0}`")]
    CannotBufferError(String),

    #[error("No such stream buffered")]
    NoSuchStreamError,

    #[error("std::io::Error: `{0}`")]
    StdIoError(String),

    #[error("`{0}`")]
    Unexpected(String),
}

/// Allows us to convert from std::io::Error to Bluefin errors. This is mostly a quality
/// of life requirement since this let's use the `?` operator with greater ease.
impl From<std::io::Error> for BluefinError {
    fn from(error: std::io::Error) -> Self {
        BluefinError::StdIoError(error.to_string())
    }
}
