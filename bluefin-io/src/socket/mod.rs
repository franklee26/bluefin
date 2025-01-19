use crate::error::{BluefinIoError, BluefinIoResult};
use libc::{c_int, setsockopt};

mod cmsghdr;
pub mod udp_socket;

pub(crate) fn set_sock_opt<T>(
    fd: c_int,
    level: c_int,
    name: c_int,
    value: T,
) -> BluefinIoResult<()> {
    unsafe {
        match setsockopt(
            fd,
            level,
            name,
            &value as *const _ as _,
            size_of_val(&value) as _,
        ) {
            0 => Ok(()),
            _ => Err(BluefinIoError::StdIoError(
                std::io::Error::last_os_error().to_string(),
            )),
        }
    }
}
