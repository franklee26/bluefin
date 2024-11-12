use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::net::{client::BluefinClient, server::BluefinServer};
use rstest::rstest;
use tokio::{
    task::JoinSet,
    time::{sleep, timeout},
};

const SERVER_IPV4_ADDR: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 38);
const CLIENT_IPV4_ADDR: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 37);

#[rstest]
#[timeout(Duration::from_secs(3))]
#[case(1318, 1319, 10)]
#[case(1320, 1321, 100)]
#[case(1322, 1323, 222)]
#[case(1324, 1325, 500)]
#[tokio::test]
async fn basic_server_client_connection_send_recv(
    #[case] client_port: u16,
    #[case] server_port: u16,
    #[case] server_read_size: usize,
) {
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        SERVER_IPV4_ADDR,
        server_port,
    )));
    server
        .bind()
        .await
        .expect("Encountered error while binding server");

    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        CLIENT_IPV4_ADDR,
        client_port,
    )));

    const TOTAL_NUM_BYTES_SENT: usize = 3000;
    const BATCH_SIZE: usize = 250;
    let mut join_set = JoinSet::new();

    join_set.spawn(async move {
        let mut conn = timeout(Duration::from_secs(5), server.accept())
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
    });

    join_set.spawn(async move {
        let mut conn = client
            .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
                SERVER_IPV4_ADDR,
                server_port,
            )))
            .await
            .expect("Client timed out waiting to connect to server");

        // Wait for 250ms for the server to be ready
        sleep(Duration::from_millis(250)).await;

        // Send TOTAL_NUM_BYTES_SENT across the wire
        let mut total_num_bytes_sent = 0;

        // Send 7 bytes
        let bytes = [1, 2, 3, 4, 5, 6, 7];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #1")
            .expect("Client encountered error while sending");
        assert_eq!(size, 7);
        total_num_bytes_sent += 7;

        // Send 50 bytes
        let bytes = [10; 50];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #2")
            .expect("Client encountered error while sending");
        assert_eq!(size, 50);
        total_num_bytes_sent += 50;

        // Send 3 bytes
        let bytes = [8, 8, 8];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #3")
            .expect("Client encountered error while sending");
        assert_eq!(size, 3);
        total_num_bytes_sent += 3;

        // Send 40 bytes
        let bytes = [99; 40];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #4")
            .expect("Client encountered error while sending");
        assert_eq!(size, 40);
        total_num_bytes_sent += 40;

        // Send 500 bytes
        let bytes = [27; 500];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #5")
            .expect("Client encountered error while sending");
        assert_eq!(size, 500);
        total_num_bytes_sent += 500;

        // Send 399 bytes
        let bytes = [18; 399];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #6")
            .expect("Client encountered error while sending");
        assert_eq!(size, 399);
        total_num_bytes_sent += 399;

        // Send 1 byte
        let bytes = [19];
        let size = timeout(Duration::from_secs(3), conn.send(&bytes))
            .await
            .expect("Client timed out while sending batch #7")
            .expect("Client encountered error while sending");
        assert_eq!(size, 1);
        total_num_bytes_sent += 1;

        // We will send 2000 bytes now in batches of 250 bytes
        for round_num in 0..8 {
            let bytes = [round_num; BATCH_SIZE];
            let size = timeout(Duration::from_secs(3), conn.send(&bytes))
                .await
                .expect(&format!(
                    "Client timed out while sending batch #{}",
                    8 + round_num
                ))
                .expect("Client encountered error while sending");
            assert_eq!(size, BATCH_SIZE);
            total_num_bytes_sent += BATCH_SIZE;
        }

        assert_eq!(total_num_bytes_sent, TOTAL_NUM_BYTES_SENT);
    });
    join_set.join_all().await;
}
