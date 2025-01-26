use bluefin::net::{client::BluefinClient, server::BluefinServer};
use core::str;
use local_ip_address::list_afinet_netifas;
use rstest::{fixture, rstest};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    time::Duration,
};
use tokio::{
    spawn,
    task::JoinSet,
    time::{sleep, timeout},
};

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

#[rstest]
#[timeout(Duration::from_secs(15))]
#[case(1318, 1319, 10)]
#[case(1320, 1321, 100)]
#[case(1322, 1323, 222)]
#[case(1324, 1325, 500)]
#[tokio::test]
async fn basic_server_client_connection_send_recv(
    loopback_ip_addr: &Ipv4Addr,
    #[case] client_port: u16,
    #[case] server_port: u16,
    #[case] server_read_size: usize,
) {
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        *loopback_ip_addr,
        server_port,
    )));
    server
        .bind()
        .await
        .expect("Encountered error while binding server");
    let _ = server.set_num_reader_workers(20);

    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        *loopback_ip_addr,
        client_port,
    )));

    const TOTAL_NUM_BYTES_SENT: usize = 3000;
    const BATCH_SIZE: usize = 250;
    let mut join_set = JoinSet::new();

    join_set.spawn(async move {
        let mut conn = timeout(Duration::from_secs(10), server.accept())
            .await
            .expect("Server timed out waiting to accept connection from client")
            .expect("Failed to create bluefin connection");

        let mut stitched_bytes: Vec<u8> = Vec::new();
        let mut total_num_bytes_read = 0;
        loop {
            // Read everything we expected to from the client
            if total_num_bytes_read == TOTAL_NUM_BYTES_SENT {
                break;
            }

            let mut buf = [0u8; 1024];
            let size = timeout(
                Duration::from_secs(5),
                conn.recv(&mut buf, server_read_size),
            )
            .await
            .expect("Server timed out waiting to recv batch #1")
            .expect("Server encountered error while calling recv");

            // Ensure we did not read more than what we specified
            assert!(size > 0);
            assert!(size <= server_read_size);

            stitched_bytes.extend_from_slice(&buf[..size]);
            total_num_bytes_read += size;
        }

        assert_eq!(total_num_bytes_read, TOTAL_NUM_BYTES_SENT);

        // Assert that the bytes we read are correct and in the correct order
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7],
            stitched_bytes[..7],
            "batch #1 not expected"
        );
        assert_eq!([10; 50], stitched_bytes[7..57], "batch #2 not expected");
        assert_eq!([8, 8, 8], stitched_bytes[57..60], "batch #3 not expected");
        assert_eq!([99; 40], stitched_bytes[60..100], "batch #4 not expected");
        assert_eq!([27; 500], stitched_bytes[100..600], "batch #5 not expected");
        assert_eq!([18; 399], stitched_bytes[600..999], "batch #6 not expected");
        assert_eq!([19], stitched_bytes[999..1000], "batch #7 not expected");

        let mut base = 1000;
        for round_num in 0..8 {
            assert_eq!(
                [round_num; 250],
                stitched_bytes[base..base + 250],
                "batch #{} not expected",
                8 + round_num
            );
            base += 250;
        }

        // Now flip around and let the server send 5 bytes
        let size = conn.send(&[5, 4, 3, 2, 1]);
        assert!(size.is_ok_and(|s| s == 5), "Failed to send bytes");

        // Send another 10 bytes
        let size = conn.send(&[2, 4, 6, 8, 10, 12, 14, 16, 18, 20]);
        assert!(size.is_ok_and(|s| s == 10), "Failed to send bytes");
    });

    let loopback_cloned = loopback_ip_addr.clone();
    join_set.spawn(async move {
        let mut conn = client
            .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                loopback_cloned,
                server_port,
            )))
            .await
            .expect("Client timed out waiting to connect to server");

        // Wait for 100ms for the server to be ready
        sleep(Duration::from_millis(100)).await;

        // Send TOTAL_NUM_BYTES_SENT across the wire
        let mut total_num_bytes_sent = 0;

        // Send 7 bytes
        let bytes = [1, 2, 3, 4, 5, 6, 7];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 7), "Failed to send bytes");
        total_num_bytes_sent += 7;

        // Send 50 bytes
        let bytes = [10; 50];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 50), "Failed to send bytes");
        total_num_bytes_sent += 50;

        // Send 3 bytes
        let bytes = [8, 8, 8];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 3), "Failed to send bytes");
        total_num_bytes_sent += 3;

        // Send 40 bytes
        let bytes = [99; 40];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 40), "Failed to send bytes");
        total_num_bytes_sent += 40;

        // Send 500 bytes
        let bytes = [27; 500];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 500), "Failed to send bytes");
        total_num_bytes_sent += 500;

        // Send 399 bytes
        let bytes = [18; 399];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 399), "Failed to send bytes");
        total_num_bytes_sent += 399;

        // Send 1 byte
        let bytes = [19];
        let size = conn.send(&bytes);
        assert!(size.is_ok_and(|s| s == 1), "Failed to send bytes");
        total_num_bytes_sent += 1;

        // We will send 2000 bytes now in batches of 250 bytes
        for round_num in 0..8 {
            let bytes = [round_num; BATCH_SIZE];
            let size = conn.send(&bytes).expect(&format!(
                "Client timed out while sending batch #{}",
                8 + round_num
            ));
            assert_eq!(size, BATCH_SIZE);
            total_num_bytes_sent += BATCH_SIZE;
        }

        assert_eq!(total_num_bytes_sent, TOTAL_NUM_BYTES_SENT);

        sleep(Duration::from_millis(100)).await;
        let mut buf = [0u8; 30];
        let size = timeout(Duration::from_secs(5), conn.recv(&mut buf, 20))
            .await
            .expect("Client timed out waiting to recv bytes")
            .expect("Client encountered error while calling recv");
        assert_eq!(size, 15);
        assert_eq!(
            buf[..size],
            [5, 4, 3, 2, 1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20]
        );
    });

    join_set.join_all().await;
}

#[rstest]
#[timeout(Duration::from_secs(15))]
#[tokio::test]
async fn basic_server_client_multiple_connections_send_recv(loopback_ip_addr: &Ipv4Addr) {
    use std::sync::Arc;

    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        *loopback_ip_addr,
        1419,
    )));
    server
        .bind()
        .await
        .expect("Encountered error while binding server");

    let mut join_set = JoinSet::new();
    const NUM_CONNECTIONS: usize = 3;
    const MAX_BYTES_SENT_PER_CONNECTION: usize = 3200;
    let client_ports: [u16; NUM_CONNECTIONS] = [1420, 1421, 1422];
    let loopback_cloned = loopback_ip_addr.clone();
    let data = Arc::new(generate_connection_date(NUM_CONNECTIONS));
    let data_cloned = Arc::clone(&data);

    join_set.spawn(async move {
        let mut conn_num = 0;
        while let Ok(mut conn) = timeout(Duration::from_secs(10), server.accept())
            .await
            .expect(&format!(
                "Server #{} timed out waiting to accept connection from client",
                conn_num
            ))
        {
            let data_cloned = Arc::clone(&data_cloned);
            spawn(async move {
                // The test will first send a key of five bytes.
                let mut key_buf: [u8; 5] = [0; 5];
                let size = timeout(Duration::from_secs(1), conn.recv(&mut key_buf, 5))
                    .await
                    .expect("Server timed out while waiting for key")
                    .expect("Server encountered error while receiving");
                assert_eq!(size, 5);
                let key = match str::from_utf8(&key_buf) {
                    Ok(s) => s,
                    Err(_) => panic!("Could not retrieve key from client"),
                };

                let expected_data = data_cloned.get(key).expect("Could not fetch expected data");
                let mut stitched_bytes: Vec<u8> = Vec::new();
                let mut buf = [0u8; 1500];
                loop {
                    let size = timeout(Duration::from_secs(1), conn.recv(&mut buf, 1500))
                        .await
                        .expect("Server timed out while waiting for data")
                        .expect("Server encountered error while receiving data");
                    assert_ne!(size, 0);
                    assert!(size <= 1500);
                    stitched_bytes.extend_from_slice(&buf[..size]);

                    if stitched_bytes.len() == MAX_BYTES_SENT_PER_CONNECTION {
                        break;
                    }
                }
                assert_eq!(stitched_bytes, *expected_data);
            });
            conn_num += 1;

            if conn_num >= NUM_CONNECTIONS {
                break;
            }
        }
    });

    for conn_num in 0..NUM_CONNECTIONS {
        // Random amount of time to sleep
        let data_cloned = Arc::clone(&data);
        let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
            loopback_cloned,
            client_ports[conn_num],
        )));

        if let Ok(mut conn) = client
            .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                loopback_cloned,
                1419,
            )))
            .await
        {
            join_set.spawn(async move {
                // Tell the server who we are by sending the key. Key is five bytes.
                let key = format!("key_{}", conn_num);
                let size = conn.send(key.as_bytes());
                assert!(size.is_ok_and(|s| s == 5), "Failed to send bytes");

                sleep(Duration::from_millis(10)).await;

                // Now begin sending the actual data in batches of 32 bytes
                let mut total_bytes_sent = 5;
                let data_to_send = data_cloned.get(&key).unwrap();
                let max_num_iterations = data_to_send.len() / 32;
                let mut start_ix = 0;
                let mut num_iterations = 0;
                while num_iterations < max_num_iterations {
                    let size = conn.send(&data_to_send[start_ix..start_ix + 32]);
                    assert!(size.is_ok_and(|s| s == 32), "Failed to send bytes");
                    start_ix += 32;
                    total_bytes_sent += 32;
                    num_iterations += 1;
                }

                assert_eq!(
                    total_bytes_sent,
                    MAX_BYTES_SENT_PER_CONNECTION + 5,
                    "Did not send the expected number of bytes"
                );
            });
            sleep(Duration::from_millis(5)).await;
        }
    }

    join_set.join_all().await;
}

fn generate_connection_date(num_connections: usize) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    for ix in 0..num_connections {
        // Push 3.2kb bytes of data per connection
        let mut data = Vec::new();
        for _ in 0..100 {
            let random_data: [u8; 32] = rand::random();
            data.extend(random_data);
        }

        let key = format!("key_{}", ix);
        map.insert(key, data);
    }
    map
}
