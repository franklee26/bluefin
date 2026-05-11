use std::net::SocketAddr;

use bluefin_io::socket::udp_socket::BluefinSocket;
use bluefin_proto::BluefinResult;
use tokio::net::UdpSocket;

pub mod common;
#[cfg(target_os = "macos")]
pub mod macos_io;
pub mod ring_buffer;
pub mod window;

#[inline]
pub(crate) fn get_udp_socket(src_addr: SocketAddr) -> BluefinResult<UdpSocket> {
    let s = get_udp_socket_impl(src_addr)?;
    let udp_sock: std::net::UdpSocket = s.into();
    let socket = udp_sock.try_into()?;

    Ok(socket)
}

#[inline]
pub(crate) fn get_connected_udp_socket(
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
) -> BluefinResult<UdpSocket> {
    let socket = get_udp_socket_impl(src_addr)?;
    socket.connect(&socket2::SockAddr::from(dst_addr))?;

    let udp_sock: std::net::UdpSocket = socket.into();
    let s = udp_sock.try_into()?;

    Ok(s)
}

#[inline]
fn get_udp_socket_impl(src_addr: SocketAddr) -> BluefinResult<socket2::Socket> {
    let udp_sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    udp_sock.set_reuse_address(true)?;
    udp_sock.set_reuse_port(true)?;
    udp_sock.set_cloexec(true)?;
    udp_sock.set_nonblocking(true).unwrap();

    // Request 32 MB recv/send buffers. The kernel will cap at
    // `kern.ipc.maxsockbuf` (8 MB on macOS by default; bump with
    // `sudo sysctl -w kern.ipc.maxsockbuf=33554432` to actually get 32 MB).
    // The default 8 MB only buffers ~1.6 ms of payload at 5 GB/s, which is
    // the headline blocker for vectorised / batched I/O.
    udp_sock.set_recv_buffer_size(32 * 1024 * 1024)?;
    udp_sock.set_send_buffer_size(32 * 1024 * 1024)?;

    udp_sock.bind(&socket2::SockAddr::from(src_addr))?;
    Ok(udp_sock)
}
