use std::{future::Future, io, os::fd::FromRawFd, pin::Pin};

use tokio::fs::File;

use crate::{
    core::error::BluefinError,
    io::{
        manager::{ConnectionManager, Result},
        read::Accept,
    },
};

use super::connection::Connection;

/// A BluefinSocket is a wrapper on top of someother file descriptor (fd). Usually
/// fd is some UDP-socket but it can also be some tun-tap network interface.
/// Because Bluefin is built on top of something connection-less like UDP, we need
/// to implement our own socket apis. In particular, we need to multiplex over
/// the single fd and ensure the incoming IO operations are redirected to the
/// correct connections.
#[derive(Debug)]
pub(crate) struct BluefinSocket {
    pub(crate) fd: i32,
    pub(crate) need_ip_and_udp_headers: bool,
    pub(crate) manager: ConnectionManager,
}

impl BluefinSocket {
    pub fn new(fd: i32, need_ip_and_udp_headers: bool) -> Self {
        let manager = ConnectionManager::new();
        Self {
            fd,
            manager,
            need_ip_and_udp_headers,
        }
    }
}
