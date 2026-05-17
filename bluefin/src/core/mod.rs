//! Wire-format types for the Bluefin protocol.
//!
//! As of slice 1a of the sans-io migration, these types live in
//! [`bluefin_proto::wire`]. This module is a thin re-export shim so
//! existing call sites that import via `crate::core::{header, packet,
//! Extract, Serialisable}` keep compiling. New code SHOULD reach for
//! `bluefin_proto::wire::*` directly.

pub use bluefin_proto::wire::{header, packet, Extract, Serialisable};
