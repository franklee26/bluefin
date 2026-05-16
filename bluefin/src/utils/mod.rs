use std::net::SocketAddr;

use bluefin_io::socket::factory::{tokio_connected_udp_socket, tokio_udp_socket};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use tokio::net::UdpSocket;

pub mod common;
#[cfg(target_os = "macos")]
pub mod macos_io;
pub mod ring_buffer;
pub mod window;

/// Thin shim: delegates to
/// [`bluefin_io::socket::factory::tokio_udp_socket`]. Slice 4 of the
/// sans-io migration consolidated socket setup into `bluefin-io`.
#[inline]
pub(crate) fn get_udp_socket(src_addr: SocketAddr) -> BluefinResult<UdpSocket> {
    tokio_udp_socket(src_addr).map_err(|e| BluefinError::Unexpected(e.to_string()))
}

/// Thin shim: delegates to
/// [`bluefin_io::socket::factory::tokio_connected_udp_socket`].
#[inline]
pub(crate) fn get_connected_udp_socket(
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
) -> BluefinResult<UdpSocket> {
    tokio_connected_udp_socket(src_addr, dst_addr)
        .map_err(|e| BluefinError::Unexpected(e.to_string()))
}
