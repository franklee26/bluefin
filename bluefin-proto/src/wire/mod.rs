//! Wire-format types for the Bluefin transport protocol.
//!
//! These types describe the bytes on the wire (header, packet) and the
//! traits used to serialise/deserialise them. They are *sans-io*: no
//! `tokio`, no socket, no async — just `&[u8]` / `bytes::Bytes` in,
//! `Vec<u8>` / `Bytes` out.
//!
//! Lifted from `bluefin::core` in slice 1a of the sans-io migration
//! (see [`docs/SANS_IO_MIGRATION.md`](../../../docs/SANS_IO_MIGRATION.md)).
//! The original module paths in `bluefin::core::{header, packet}` remain
//! as `pub use` re-exports for source compatibility.

use crate::error::BluefinError;

pub mod header;
pub mod packet;

/// Replace `self` with `Default::default()` and return the previous value.
/// Useful for moving a payload out of an `Option<...>`-shaped buffer slot
/// without allocating.
pub trait Extract: Default {
    fn extract(&mut self) -> Self;
}

impl<T: Default> Extract for T {
    #[inline]
    fn extract(&mut self) -> Self {
        std::mem::replace(self, T::default())
    }
}

/// Wire-format serialisation contract. Implemented by [`header::BluefinHeader`],
/// [`header::BluefinSecurityFields`], and [`packet::BluefinPacket`].
pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized;
}
