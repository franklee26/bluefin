use std::io::ErrorKind;
use thiserror::Error;

pub type BluefinIoResult<T> = Result<T, BluefinIoError>;

#[derive(Error, Debug, PartialEq)]
pub enum BluefinIoError {
    #[error("std::io::Error: `{0}`")]
    StdIoError(String),

    #[error("`{0}`")]
    InsufficientBufferSize(String),

    #[error("`{0}`")]
    Unsupported(String),
}

impl From<std::io::Error> for BluefinIoError {
    fn from(error: std::io::Error) -> Self {
        BluefinIoError::StdIoError(error.to_string())
    }
}

impl From<ErrorKind> for BluefinIoError {
    fn from(error: ErrorKind) -> Self {
        BluefinIoError::StdIoError(error.to_string())
    }
}
