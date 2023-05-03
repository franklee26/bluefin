use std::{result, task::Waker};

use crate::core::error::BluefinError;

pub mod buffered_read;
pub mod buffered_write;
pub mod manager;
pub mod stream_manager;
pub mod worker;

/// Bluefin Result yields a BluefinError
pub type Result<T> = result::Result<T, BluefinError>;

/// Helper macro for setting the waker for the `Buffer` trait. This is only needed because in Rust we
/// can't assert that all structs implementing the trait has a `waker` field.
#[macro_export]
macro_rules! set_waker {
    () => {
        /// Sets the waker, default implementation
        #[inline]
        fn set_waker(&mut self, waker: Option<std::task::Waker>) {
            self.waker = waker;
        }
    };
}

/// Bluefin Buffer used for connection or stream-based data buffering
pub trait Buffer {
    /// Data type we are trying to buffer
    type BufferData: Sized;
    /// Data yielded when we consume from the buffer. This may or may not be the same as `BufferData`
    type ConsumedData: Sized;

    /// Defines how to buffer or add in data
    fn add(&mut self, data: Self::BufferData) -> Result<()>;

    /// Defines how to consume data from the buffer. Notice that this takes
    /// ownership over the `BufferData`
    fn consume(&mut self) -> Option<Self::ConsumedData>;

    /// All buffers need a way to modify its associated waker such that any awaiting
    /// future(s) can be woken up.
    fn set_waker(&mut self, waker: Option<Waker>);
}
