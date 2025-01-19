use crate::error::{BluefinIoError, BluefinIoResult};
use crate::socket::cmsghdr::CmsghdrBufferHandler;
use crate::socket::set_sock_opt;
use libc::{c_int, sockaddr_storage};
use std::cmp::min;
use std::io::IoSliceMut;
use std::mem::{zeroed, MaybeUninit};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd;
use std::task::{Context, Poll};
use std::{io, mem};
use tokio::net::UdpSocket;

pub struct BluefinSocket(UdpSocket);

/// Aligns `T` to 8 byte boundaries
#[derive(Clone, Copy)]
#[repr(align(8))]
pub(crate) struct Align<T>(T);

// This is just a guess.
const MSG_CONTROLLEN: usize = 8 * 10;
const IOVEC_SIZE: usize = 8;

pub struct BufMetadata {
    pub src_addr: SocketAddr,
    pub len: usize,
}

impl Default for BufMetadata {
    fn default() -> BufMetadata {
        Self {
            src_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            len: 0,
        }
    }
}

pub struct TransmitData<'a> {
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
    data: &'a [u8],
}

#[cfg(feature = "macos-fast")]
#[repr(C)]
pub(crate) struct msghdr_x {
    pub msg_name: *mut libc::c_void,
    pub msg_namelen: libc::socklen_t,
    pub msg_iov: *mut libc::iovec,
    pub msg_iovlen: c_int,
    pub msg_control: *mut libc::c_void,
    pub msg_controllen: libc::socklen_t,
    pub msg_flags: c_int,
    pub msg_datalen: usize,
}

#[cfg(feature = "macos-fast")]
extern "C" {
    fn recvmsg_x(s: c_int, msgp: *const msghdr_x, cnt: libc::c_uint, flags: c_int) -> isize;

    fn sendmsg_x(s: c_int, msgp: *const msghdr_x, cnt: libc::c_uint, flags: c_int) -> isize;
}

impl TransmitData<'_> {
    pub fn new(src_addr: SocketAddr, dst_addr: SocketAddr, data: &[u8]) -> TransmitData {
        TransmitData {
            src_addr,
            dst_addr,
            data,
        }
    }
}

impl BluefinSocket {
    pub fn new(src_addr: SocketAddr) -> BluefinIoResult<Self> {
        let udp_sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        udp_sock.set_reuse_address(true)?;
        udp_sock.set_reuse_port(true)?;
        udp_sock.set_cloexec(true)?;
        udp_sock.set_nonblocking(true)?;
        udp_sock.bind(&socket2::SockAddr::from(src_addr))?;

        let fd = udp_sock.as_raw_fd();
        set_sock_opt(fd, libc::IPPROTO_IP, libc::IP_RECVTOS, 1)?;
        set_sock_opt(fd, libc::IPPROTO_IP, libc::IP_RECVDSTADDR, 1)?;

        let udp_sock: std::net::UdpSocket = udp_sock.into();
        let socket = udp_sock.try_into()?;
        Ok(Self(socket))
    }

    pub fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        metadata_bufs: &mut [BufMetadata],
    ) -> Poll<usize> {
        loop {
            // Socket not ready to begin recv. Pass in the context so that when the socket
            // is ready, the calling future can wake this up.
            if self.0.poll_recv_ready(cx).is_pending() {
                return Poll::Pending;
            }
            eprintln!("Ready!");
            return match self.poll_recv_impl(self.0.as_raw_fd(), bufs, metadata_bufs) {
                Ok(num_msgs) => Poll::Ready(num_msgs),
                Err(e) => {
                    eprintln!("Encountered error while polling socket: {}", e);
                    Poll::Pending
                }
            };
        }
    }

    #[cfg(not(feature = "macos-fast"))]
    pub fn send_to(&self, transmit_data: &TransmitData) -> BluefinIoResult<usize> {
        let msghdr = self.init_buf_for_send(transmit_data)?;
        loop {
            let size = unsafe { libc::sendmsg(self.0.as_raw_fd(), &msghdr, 0) };
            // Successful send! Returns number of characters sent.
            if size >= 0 {
                return Ok(size as _);
            }

            // Else, we must have encountered an error. Let's see if we can recover.
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                _ => return Err(BluefinIoError::from(err)),
            }
        }
    }

    #[cfg(feature = "macos-fast")]
    pub fn send_to(&self, transmit_data: &TransmitData) -> BluefinIoResult<usize> {
        let msghdr = self.init_buf_for_send(transmit_data)?;
        loop {
            let size = unsafe { libc::sendmsg(self.0.as_raw_fd(), &msghdr, 0) };
            // Successful send! Returns number of characters sent.
            if size >= 0 {
                return Ok(size as _);
            }

            // Else, we must have encountered an error. Let's see if we can recover.
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                _ => return Err(BluefinIoError::from(err)),
            }
        }
    }

    // Returns number of buffers filled
    #[cfg(not(feature = "macos-fast"))]
    fn poll_recv_impl(
        &self,
        fd: c_int,
        bufs: &mut [IoSliceMut<'_>],
        metadata_bufs: &mut [BufMetadata],
    ) -> BluefinIoResult<usize> {
        // Use sockaddr_storage as it is designed to fit both sockaddr_in and sockaddr_in6. We can
        // apparently type cast this to sockaddr later.
        let mut msg_name = MaybeUninit::<sockaddr_storage>::uninit();
        let mut msghdr = self.init_buf(&mut bufs[0], &mut msg_name)?;

        let num_bytes = loop {
            // TODO:
            // Despite vectored io, we can only make use of one buf?
            // Reproduced from recvmsg(2)
            // https://man7.org/linux/man-pages/man2/recvmsg.2.html
            //
            // ssize_t recvmsg(int sockfd, struct msghdr *msg, int flags);
            // According to the manpage this syscall 'return(s) the length of the message
            // on successful completion'.
            let n = unsafe { libc::recvmsg(fd, &mut msghdr, 0) };
            if n == -1 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(BluefinIoError::from(e));
            }
            if msghdr.msg_flags & libc::MSG_TRUNC != 0 {
                continue;
            }
            break n;
        };

        // Write the metadata for the one buffer we filled
        metadata_bufs[0] = self.get_metadata_buf(&msg_name, num_bytes as _)?;

        // Upon success, we touch just one buffer
        Ok(1)
    }

    #[cfg(feature = "macos-fast")]
    fn poll_recv_impl(
        &self,
        fd: c_int,
        bufs: &mut [IoSliceMut<'_>],
        metadata_bufs: &mut [BufMetadata],
    ) -> BluefinIoResult<usize> {
        let mut msg_names = [MaybeUninit::<sockaddr_storage>::uninit(); IOVEC_SIZE];
        let mut msghdrxs = unsafe { zeroed::<[msghdr_x; IOVEC_SIZE]>() };
        let mut msg_controls = [Align(MaybeUninit::<[u8; MSG_CONTROLLEN]>::uninit()); IOVEC_SIZE];
        let size = min(bufs.len(), IOVEC_SIZE);
        for ix in 0..size {
            self.init_buf(
                &mut bufs[ix],
                &mut msg_names[ix],
                &mut msghdrxs[ix],
                &mut msg_controls[ix],
            )?
        }

        let num_bytes = loop {
            // TODO:
            // Despite vectored io, we can only make use of one buf?
            // Reproduced from recvmsg(2)
            // https://man7.org/linux/man-pages/man2/recvmsg.2.html
            //
            // ssize_t recvmsg(int sockfd, struct msghdr *msg, int flags);
            // According to the manpage this syscall 'return(s) the length of the message
            // on successful completion'.
            let n = unsafe { recvmsg_x(fd, msghdrxs.as_mut_ptr(), size as _, 0) };
            if n == -1 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(BluefinIoError::from(e));
            }
            break n;
        };

        // Write the metadata for the one buffer we filled
        // metadata_bufs[0] = self.get_metadata_buf(&msg_name, num_bytes as _)?;

        // Upon success, we touch just one buffer
        Ok(1)
    }

    /// Reproduced from recvmsg(2)
    /// https://man7.org/linux/man-pages/man2/recvmsg.2.html
    ///
    /// struct msghdr {
    ///    void             *msg_name;       /* Optional address */
    ///    socklen_t        msg_namelen;     /* Size of address */
    ///    struct iovec     *msg_iov;        /* Scatter/gather array */
    ///    size_t           msg_iovlen;      /* # elements in msg_iov */
    ///    void             *msg_control;    /* Ancillary data, see below */
    ///    size_t           msg_controllen;  /* Ancillary data buffer len */
    ///    int              msg_flags;       /* Flags on received message */
    /// };
    ///
    #[cfg(not(feature = "macos-fast"))]
    #[inline]
    fn init_buf(
        &self,
        buf: &mut IoSliceMut,
        msg_name: &mut MaybeUninit<sockaddr_storage>,
    ) -> BluefinIoResult<libc::msghdr> {
        // TODO:
        // I'm not reading from this? Seems to have info like the intended dst_ip. How important
        // is this for now?
        let mut msg_control = Align(MaybeUninit::<[u8; MSG_CONTROLLEN]>::uninit());
        let mut msghdr = unsafe { mem::zeroed::<libc::msghdr>() };

        msghdr.msg_name = msg_name.as_mut_ptr() as _;
        msghdr.msg_namelen = size_of::<sockaddr_storage>() as _;
        msghdr.msg_iov = buf as *mut IoSliceMut as *mut libc::iovec;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = msg_control.0.as_mut_ptr() as _;
        msghdr.msg_controllen = MSG_CONTROLLEN as _;
        msghdr.msg_flags = 0;

        Ok(msghdr)
    }

    #[cfg(feature = "macos-fast")]
    #[inline]
    fn init_buf(
        &self,
        buf: &mut IoSliceMut,
        msg_name: &mut MaybeUninit<sockaddr_storage>,
        msghdr_x: &mut msghdr_x,
        msg_control: &mut Align<MaybeUninit<[u8; MSG_CONTROLLEN]>>,
    ) -> BluefinIoResult<()> {
        msghdr_x.msg_name = msg_name.as_mut_ptr() as _;
        msghdr_x.msg_namelen = size_of::<sockaddr_storage>() as _;
        msghdr_x.msg_iov = buf as *mut IoSliceMut as *mut libc::iovec;
        msghdr_x.msg_iovlen = 1;
        msghdr_x.msg_control = msg_control.0.as_mut_ptr() as _;
        msghdr_x.msg_controllen = MSG_CONTROLLEN as _;
        msghdr_x.msg_flags = 0;
        msghdr_x.msg_controllen = buf.len() as _;

        Ok(())
    }

    #[inline]
    fn get_metadata_buf(
        &self,
        msg_name: &MaybeUninit<sockaddr_storage>,
        len: usize,
    ) -> BluefinIoResult<BufMetadata> {
        // According to recvmsg(2):
        // The msg_name field points to a caller-allocated buffer that is used to return the
        // source address if the socket is unconnected.

        // SAFETY:
        // msg_name is always initialised and populated.
        let msg_name_unwrapped = unsafe { msg_name.assume_init() };
        eprintln!(
            "ss_len: {}, ss_family: {}",
            msg_name_unwrapped.ss_len, msg_name_unwrapped.ss_family
        );
        let src_addr = match c_int::from(msg_name_unwrapped.ss_family) {
            libc::AF_INET => {
                // SAFETY:
                // If ss_family is AF_INET then the sockaddr_storage must represent an ipv4 address
                // and can therefore always be cast to sockaddr_in.
                let addr: &libc::sockaddr_in =
                    unsafe { &*(&msg_name_unwrapped as *const _ as *const libc::sockaddr_in) };
                SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
                    u16::from_be(addr.sin_port),
                ))
            }
            // TODO:
            // No ipv6 support.
            ss_family => {
                return Err(BluefinIoError::StdIoError(format!(
                    "Unknown ss_family received: {}",
                    ss_family
                )))
            }
        };

        Ok(BufMetadata { src_addr, len })
    }

    /// Reproduced from recvmsg(2)
    /// https://man7.org/linux/man-pages/man2/recvmsg.2.html
    ///
    /// struct msghdr {
    ///    void             *msg_name;       /* Optional address */
    ///    socklen_t        msg_namelen;     /* Size of address */
    ///    struct iovec     *msg_iov;        /* Scatter/gather array */
    ///    size_t           msg_iovlen;      /* # elements in msg_iov */
    ///    void             *msg_control;    /* Ancillary data, see below */
    ///    size_t           msg_controllen;  /* Ancillary data buffer len */
    ///    int              msg_flags;       /* Flags on received message */
    /// };
    ///
    fn init_buf_for_send(&self, transmit_data: &TransmitData<'_>) -> BluefinIoResult<libc::msghdr> {
        let mut msghdr: libc::msghdr = unsafe { mem::zeroed() };
        let mut msg_iov: libc::iovec = unsafe { mem::zeroed() };
        let mut msg_control = Align([0u8; MSG_CONTROLLEN]);

        // Set the actual data to be sent. Placed in msg_iov
        msg_iov.iov_base = transmit_data.data.as_ptr() as _;
        msg_iov.iov_len = transmit_data.data.len() as _;
        msghdr.msg_iov = &mut msg_iov as *mut _;
        msghdr.msg_iovlen = 1;

        // Set the msg_control. We need to do this before using the handler to add data.
        msghdr.msg_control = msg_control.0.as_mut_ptr() as _;
        msghdr.msg_controllen = MSG_CONTROLLEN as _;

        // To add ancillary data, we must use the cmsghdr handler.
        let mut cmsg_handler = CmsghdrBufferHandler::new(&msghdr);
        match transmit_data.src_addr.ip() {
            IpAddr::V4(addr) => {
                let libc_in_addr = libc::in_addr {
                    s_addr: u32::from_ne_bytes(addr.octets()),
                };
                cmsg_handler.append(libc::IPPROTO_IP, libc::IP_RECVDSTADDR, libc_in_addr)?;
            }
            _ => {
                return Err(BluefinIoError::Unsupported(
                    "Non ipv4 addr are currently unsupported".to_string(),
                ))
            }
        }

        // The socket is not connected so the dst addr is needed. This is set in the msg_name field
        // and the length of this data is in msg_namelen.
        // Cast this to socket2::SockAddr so we can get a pointer to it.
        let dst_addr: &socket2::SockAddr = &transmit_data.dst_addr.into();
        let dst_addr_ptr = dst_addr.as_ptr();
        msghdr.msg_name = dst_addr_ptr as *mut _;
        msghdr.msg_namelen = dst_addr.len();

        Ok(msghdr)
    }
}
