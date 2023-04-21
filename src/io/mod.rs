use std::{result, task::Waker};

use crate::core::{error::BluefinError, packet::Packet};

pub mod buffered_read;
pub mod manager;
pub mod stream_manager;
pub mod worker;

/// Bluefin Result yields a BluefinError
pub type Result<T> = result::Result<T, BluefinError>;
