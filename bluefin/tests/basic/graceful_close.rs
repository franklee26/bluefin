use bluefin::net::{client::BluefinClient, server::BluefinServer};
use local_ip_address::list_afinet_netifas;
use rstest::{fixture, rstest};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    time::Duration,
};
use tokio::{task::JoinSet, time::timeout};

#[cfg(target_os = "macos")]
fn loopback_interface_name() -> &'static str {
    "lo0"
}

#[cfg(target_os = "linux")]
fn loopback_interface_name() -> &'static str {
    "lo"
}

#[fixture]
#[once]
#[inline]
fn loopback_ip_addr() -> Ipv4Addr {
    let network_interfaces = list_afinet_netifas().unwrap();
    let mut ip_addr: Option<IpAddr> = None;
    for (name, ip) in network_interfaces.iter() {
        if name == loopback_interface_name() {
            ip_addr = Some(*ip);
            break;
        }
    }
    if ip_addr.is_none() {
        panic!("Could not find loopback address");
    }
    match ip_addr.unwrap() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => panic!("Unexpectedly received ipv6"),
    }
}

/// End-to-end exercise of the FIN / FIN-ACK exchange:
///
/// 1. Server accepts; client connects.
/// 2. Client sends a small payload; server receives it via `recv`.
/// 3. Client calls `close()`. Internally this flushes, sends a `Fin`, and
///    awaits the server's `FinAck`.
/// 4. Server's blocked `recv` unparks and returns `Ok(0)` (EOF) once the
///    `Fin` is processed.
/// 5. Client's `close()` resolves and `is_closed()` reports true.
#[rstest]
#[timeout(Duration::from_secs(15))]
#[tokio::test]
async fn graceful_close_drives_eof_on_recv_and_resolves_close(
    loopback_ip_addr: &Ipv4Addr,
) {
    let server_port: u16 = 1480;
    let client_port: u16 = 1481;

    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        *loopback_ip_addr,
        server_port,
    )));
    server.bind().await.expect("bind server");
    let _ = server.set_num_reader_workers(4);

    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        *loopback_ip_addr,
        client_port,
    )));

    let mut join_set = JoinSet::new();

    // Server task: read 5 bytes, then expect EOF (Ok(0)).
    join_set.spawn(async move {
        let mut conn = timeout(Duration::from_secs(5), server.accept())
            .await
            .expect("server accept timed out")
            .expect("server accept failed");

        let mut buf = [0u8; 32];
        let n = timeout(Duration::from_secs(5), conn.recv(&mut buf, 5))
            .await
            .expect("server recv timed out")
            .expect("server recv failed");
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], &[1, 2, 3, 4, 5]);

        // Next recv should observe EOF after the client closes.
        let n = timeout(Duration::from_secs(5), conn.recv(&mut buf, 32))
            .await
            .expect("server recv (EOF) timed out")
            .expect("server recv (EOF) failed");
        assert_eq!(n, 0, "expected EOF (Ok(0)) after client close");
    });

    let loopback_cloned = *loopback_ip_addr;
    join_set.spawn(async move {
        let mut conn = client
            .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                loopback_cloned,
                server_port,
            )))
            .await
            .expect("client connect failed");

        let n = conn.send(&[1, 2, 3, 4, 5]).expect("client send failed");
        assert_eq!(n, 5);

        // Graceful close: flushes, sends FIN, awaits FinAck from server.
        timeout(Duration::from_secs(5), conn.close())
            .await
            .expect("client close timed out")
            .expect("client close failed");

        assert!(conn.is_closed(), "client connection should report closed");

        // Subsequent send returns ConnectionClosed.
        let err = conn.send(&[9, 9, 9]).expect_err("send after close must err");
        assert!(
            matches!(err, bluefin_proto::error::BluefinError::ConnectionClosed),
            "expected ConnectionClosed, got {:?}",
            err
        );
    });

    while let Some(res) = join_set.join_next().await {
        res.expect("task panicked");
    }
}
