//! Apple's undocumented batched datagram I/O syscalls — the macOS equivalent
//! of Linux's `recvmmsg(2)`/`sendmmsg(2)`.
//!
//! The kernel symbols `recvmsg_x` / `sendmsg_x` have been stable since macOS
//! 10.7 (used by Wireshark, libcurl, picoquic, msquic, mvfst, neqo, quinn).
//! They are not in the libc man pages, but the ABI is fixed by the in-tree
//! header `bsd/sys/socket_private.h` in xnu and is widely depended on.
//!
//! There is an older `#[cfg(macos_fast)]` scaffold of this in
//! `bluefin-io/src/socket/udp_socket.rs`, but `BluefinSocket` is dead in the
//! runtime path (see `bluefin/src/utils/mod.rs`, which builds a raw
//! `socket2::Socket` and converts straight to `tokio::net::UdpSocket`). This
//! module is intentionally narrow: just the batch syscall wrappers, called
//! directly on the FD that tokio hands us.

#![cfg(target_os = "macos")]

use libc::{c_int, c_uint, iovec, sockaddr_storage, socklen_t};
use std::io;
use std::os::fd::RawFd;

/// Apple's per-message header for `recvmsg_x`/`sendmsg_x`. Matches
/// `struct msghdr_x` in xnu's `bsd/sys/socket_private.h`. Layout differs
/// from POSIX `msghdr` only by the trailing `msg_datalen` field (set on
/// recv, ignored on send).
#[repr(C)]
pub(crate) struct MsghdrX {
    pub msg_name: *mut libc::c_void,
    pub msg_namelen: socklen_t,
    pub msg_iov: *mut iovec,
    pub msg_iovlen: c_int,
    pub msg_control: *mut libc::c_void,
    pub msg_controllen: socklen_t,
    pub msg_flags: c_int,
    /// Bytes received in this message; written by the kernel on `recvmsg_x`,
    /// ignored by `sendmsg_x`.
    pub msg_datalen: usize,
}

extern "C" {
    fn recvmsg_x(s: c_int, msgp: *mut MsghdrX, cnt: c_uint, flags: c_int) -> isize;
    fn sendmsg_x(s: c_int, msgp: *const MsghdrX, cnt: c_uint, flags: c_int) -> isize;
}

/// Batch-send `datagrams.len()` UDP datagrams in one syscall on a connected
/// UDP socket (msg_name=NULL, msg_namelen=0). Returns the number of datagrams
/// the kernel accepted (`>= 1` on success). On `EAGAIN`/`EWOULDBLOCK` returns
/// `io::ErrorKind::WouldBlock` so the caller can integrate with tokio's
/// readiness machinery via `try_io`.
///
/// SAFETY: `fd` must be a valid open UDP socket FD. Each `&[u8]` in
/// `datagrams` must live for the duration of the call (caller's stack frame
/// holds them).
#[inline]
pub(crate) fn sendmsg_x_connected(fd: RawFd, datagrams: &[&[u8]]) -> io::Result<usize> {
    debug_assert!(!datagrams.is_empty());
    debug_assert!(datagrams.len() <= MAX_BATCH);

    // Stack-local arrays. Sized at the compile-time max so we never allocate
    // and the indexing below has no bounds check (we only touch [..n]).
    let mut iovs: [iovec; MAX_BATCH] = unsafe { std::mem::zeroed() };
    let mut hdrs: [MsghdrX; MAX_BATCH] = unsafe { std::mem::zeroed() };

    let n = datagrams.len();
    for i in 0..n {
        iovs[i].iov_base = datagrams[i].as_ptr() as *mut _;
        iovs[i].iov_len = datagrams[i].len();
        // Connected socket: msg_name/namelen stay zero (already zeroed).
        hdrs[i].msg_iov = &mut iovs[i] as *mut iovec;
        hdrs[i].msg_iovlen = 1;
    }

    // SAFETY: `hdrs[..n]` is a contiguous, fully-initialised slice of
    // MsghdrX values pointing at iovecs that point at live datagram bytes
    // owned by the caller. `fd` is a valid open socket. `cnt = n` matches
    // the slice length.
    let rc = unsafe { sendmsg_x(fd, hdrs.as_ptr(), n as c_uint, 0) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

/// Batch-receive up to `bufs.len()` UDP datagrams in one syscall. Returns the
/// number of datagrams filled (`>= 1` on success); per-datagram length is
/// written to `lens[i]`. `EAGAIN`/`EWOULDBLOCK` is mapped to
/// `io::ErrorKind::WouldBlock`.
///
/// `bufs[i]` must be a valid mutable buffer big enough to hold one Bluefin
/// datagram. `lens` must be at least `bufs.len()` long.
///
/// SAFETY: `fd` must be a valid open UDP socket FD. The buffers must live
/// for the duration of the call.
#[inline]
pub(crate) fn recvmsg_x_into(
    fd: RawFd,
    bufs: &mut [&mut [u8]],
    lens: &mut [usize],
) -> io::Result<usize> {
    debug_assert!(!bufs.is_empty());
    debug_assert!(bufs.len() <= MAX_BATCH);
    debug_assert!(lens.len() >= bufs.len());

    let n = bufs.len();
    let mut iovs: [iovec; MAX_BATCH] = unsafe { std::mem::zeroed() };
    let mut hdrs: [MsghdrX; MAX_BATCH] = unsafe { std::mem::zeroed() };
    // We don't read peer addresses on the connected-socket recv path, but
    // recvmsg_x still wants somewhere to write them on unconnected sockets;
    // hand it scratch space to be safe. Zeroed/uninit is fine — we never read.
    let mut names: [sockaddr_storage; MAX_BATCH] = unsafe { std::mem::zeroed() };

    for i in 0..n {
        iovs[i].iov_base = bufs[i].as_mut_ptr() as *mut _;
        iovs[i].iov_len = bufs[i].len();
        hdrs[i].msg_name = &mut names[i] as *mut sockaddr_storage as *mut _;
        hdrs[i].msg_namelen = std::mem::size_of::<sockaddr_storage>() as socklen_t;
        hdrs[i].msg_iov = &mut iovs[i] as *mut iovec;
        hdrs[i].msg_iovlen = 1;
    }

    // SAFETY: as above; all pointers point into stack frames that outlive
    // the syscall, all index ranges are bounded by `n`.
    let rc = unsafe { recvmsg_x(fd, hdrs.as_mut_ptr(), n as c_uint, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    let count = rc as usize;
    debug_assert!(count <= n);
    for i in 0..count {
        lens[i] = hdrs[i].msg_datalen;
    }
    Ok(count)
}

/// Maximum batch size for a single syscall. Apple's docs (such as they are)
/// suggest 16 is safe; we cap at the writer's existing batch ceiling (12)
/// so the stack arrays stay small (~600 B). Bumping this is cheap if the
/// writer ever produces larger bursts.
pub(crate) const MAX_BATCH: usize = 16;
