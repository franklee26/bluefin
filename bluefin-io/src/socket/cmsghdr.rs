use crate::error::{BluefinIoError, BluefinIoResult};
use libc::c_int;

// This is a wrapper on the following structure:
// see: https://man7.org/linux/man-pages/man3/cmsg.3.html
// struct cmsghdr {
//      size_t cmsg_len;    /* Data byte count, including header
//                             (type is socklen_t in POSIX) */
//      int    cmsg_level;  /* Originating protocol */
//      int    cmsg_type;   /* Protocol-specific type */
//      /* followed by
//      unsigned char cmsg_data[]; */
//  };
pub(crate) struct CmsghdrBufferHandler<'a> {
    // This is a ref to the msghdr struct which holds this cmsghdr buffer. We need to keep this
    // reference because the CMSG_NXTHDR() requires the msghdr as an input parameter.
    src_msghdr: &'a libc::msghdr,
    // Mutable reference to the first cmsghdr in the ancillary data buffer. This value
    // must be derived from the CMSG_FIRSTHDR() macro defined in cmsg (3).
    // According to the man pages, this pointer can be null if there is not enough space allocated.
    // We will need to use the size + total_bytes fields below to safely unwrap this value.
    // The `append` call is responsible for advancing the pointer to the next free address if
    // more data was successfully appended.
    cmsghdr_ptr: Option<&'a mut libc::cmsghdr>,
    // The amount of bytes we have written to the ancillary buffer. This must not exceed
    // the `total_bytes` value below.
    size: usize,
    // Total bytes allocated to the ancillary data buffer.
    total_bytes: usize,
}

impl<'a> CmsghdrBufferHandler<'a> {
    pub(crate) fn new(msghdr: &'a libc::msghdr) -> Self {
        let cmsghdr_ptr = unsafe { libc::CMSG_FIRSTHDR(msghdr).as_mut() };
        Self {
            src_msghdr: msghdr,
            cmsghdr_ptr,
            size: 0,
            total_bytes: msghdr.msg_controllen as _,
        }
    }

    pub(crate) fn append<T>(
        &mut self,
        cmsg_level: c_int,
        cmsg_type: c_int,
        cmsg_data: T,
    ) -> BluefinIoResult<()> {
        // Need to use CMSG_SPACE macro to determine how much space cmsg_data would occupy
        let val_size: usize = unsafe { libc::CMSG_SPACE(size_of_val(&cmsg_data) as _) } as _;
        if self.size + val_size > self.total_bytes {
            return Err(BluefinIoError::InsufficientBufferSize(
                "Could not add value to ancillary data as data exceeds available free space"
                    .to_string(),
            ));
        }

        // Since there is enough space, we can get unwrap safely and get the mutable ref.
        let cmsghdr = self.cmsghdr_ptr.take().unwrap();

        // Set the level, type and len.
        cmsghdr.cmsg_level = cmsg_level;
        cmsghdr.cmsg_type = cmsg_type;
        // size_of_val() is insufficient here. As stated in cmsg(3), we need to consider alignment.
        cmsghdr.cmsg_len = unsafe { libc::CMSG_LEN(size_of_val(&cmsg_data) as _) };

        // Now finally write the data
        // First use CMSG_DATA() to get a pointer to the data portion of cmsghdr.
        let cmsghdr_data_ptr = unsafe { libc::CMSG_DATA(cmsghdr) };
        // Now, write the data.
        unsafe {
            // Need to cast this to the correct pointer type
            std::ptr::write(cmsghdr_data_ptr as *mut T, cmsg_data);
        };

        // Account for the space we used
        self.size += val_size;

        // Finally, we must advance self.cmsghdr_ptr. As directed in the man pages we will use
        // CMSG_NXTHDR().
        let next_cmsghdr = unsafe { libc::CMSG_NXTHDR(self.src_msghdr, cmsghdr).as_mut() };
        self.cmsghdr_ptr = next_cmsghdr;
        Ok(())
    }
}
