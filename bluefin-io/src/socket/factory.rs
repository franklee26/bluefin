//! Factory functions that produce a [`tokio::net::UdpSocket`] configured
//! per Bluefin's runtime needs (non-blocking, `SO_REUSE{ADDR,PORT}`,
//! `O_CLOEXEC`, large `SO_{RCV,SND}BUF`).
//!
//! Lives in `bluefin-io` so the runtime crate (`bluefin/`) and any
//! out-of-tree consumer share the same socket-setup decisions. Lifted
//! from `bluefin/src/utils/mod.rs` in migration slice 4 (the "swap"
//! consolidating socket ownership into `bluefin-io`).

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use crate::error::{BluefinIoError, BluefinIoResult};

/// Default `SO_RCVBUF` / `SO_SNDBUF` request in bytes. The kernel caps at
/// `kern.ipc.maxsockbuf` on macOS (default ~8 MB; bump with
/// `sudo sysctl -w kern.ipc.maxsockbuf=33554432` for the full 32 MB) and
/// at `/proc/sys/net/core/{r,w}mem_max` on Linux. 32 MB matches Bluefin's
/// bench-tuned default; the kernel-clamped effective size is what
/// ultimately matters.
pub const DEFAULT_SOCKET_BUF_BYTES: usize = 32 * 1024 * 1024;

#[inline]
fn make_socket2(src_addr: SocketAddr) -> BluefinIoResult<socket2::Socket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_reuse_address(true)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_reuse_port(true)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_cloexec(true)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_nonblocking(true)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_recv_buffer_size(DEFAULT_SOCKET_BUF_BYTES)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.set_send_buffer_size(DEFAULT_SOCKET_BUF_BYTES)
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    s.bind(&socket2::SockAddr::from(src_addr))
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    Ok(s)
}

#[inline]
fn into_tokio(s: socket2::Socket) -> BluefinIoResult<UdpSocket> {
    let std_sock: std::net::UdpSocket = s.into();
    UdpSocket::try_from(std_sock).map_err(|e| BluefinIoError::StdIoError(e.to_string()))
}

/// Build a bound, non-blocking [`tokio::net::UdpSocket`] suitable for the
/// Bluefin server / client RX path.
#[inline]
pub fn tokio_udp_socket(src_addr: SocketAddr) -> BluefinIoResult<UdpSocket> {
    into_tokio(make_socket2(src_addr)?)
}

/// Build a bound + connected non-blocking [`tokio::net::UdpSocket`]
/// suitable for the per-connection TX path (`socket.try_send` /
/// `socket.writable().await`).
#[inline]
pub fn tokio_connected_udp_socket(
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
) -> BluefinIoResult<UdpSocket> {
    let s = make_socket2(src_addr)?;
    s.connect(&socket2::SockAddr::from(dst_addr))
        .map_err(|e| BluefinIoError::StdIoError(e.to_string()))?;
    into_tokio(s)
}
