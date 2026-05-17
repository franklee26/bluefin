use crate::error::BluefinError;

pub mod connection;
pub mod context;
pub mod endpoint;
pub mod error;
pub mod wire;

pub type BluefinResult<T> = Result<T, BluefinError>;
