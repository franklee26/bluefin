use crate::error::BluefinError;

pub mod context;
pub mod error;
pub mod handshake;

pub type BluefinResult<T> = Result<T, BluefinError>;
