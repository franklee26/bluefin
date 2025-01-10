use bluefin_io::error::BluefinIoResult;
use bluefin_io::socket::udp_socket::{BluefinSocket, BufMetadata, TransmitData};
use local_ip_address::list_afinet_netifas;
use rand::{rng, Rng};
use rstest::{fixture, rstest};
use std::future::Future;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

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

// Example wrapper on the Bluefin socket for testing purposes. We implement recv + send_to apis
// to mimic the behaviour of a standard posix socket.
struct BluefinSocketRead(BluefinSocket);

impl BluefinSocketRead {
    fn new(bind_addr: SocketAddr) -> BluefinSocketRead {
        let socket = BluefinSocket::new(bind_addr).expect("Could not create bluefin socket");
        BluefinSocketRead(socket)
    }

    fn recv<'a>(
        &'a self,
        buf: &'a mut [IoSliceMut<'a>],
        buf_metadata: &'a mut [BufMetadata],
    ) -> Recv<'a> {
        Recv(RecvInner {
            socket: &self.0,
            buf,
            buf_metadata,
        })
    }

    fn send_to(&self, transmit_data: &TransmitData) -> BluefinIoResult<usize> {
        self.0.send_to(transmit_data)
    }
}

// Example future for receiving data on the socket.
struct Recv<'a>(RecvInner<'a>);

struct RecvInner<'a> {
    socket: &'a BluefinSocket,
    buf: &'a mut [IoSliceMut<'a>],
    buf_metadata: &'a mut [BufMetadata],
}

impl Future for Recv<'_> {
    type Output = usize;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.0;
        match inner.socket.poll_recv(cx, inner.buf, inner.buf_metadata) {
            Poll::Pending => Poll::Pending,
            // For these tests, we don't really care about the number of scatter buffers
            // we ended up touching.
            Poll::Ready(_) => Poll::Ready(inner.buf_metadata[0].len),
        }
    }
}

#[rstest]
#[timeout(Duration::from_secs(5))]
#[tokio::test]
async fn basic_bluefin_socket_read_write(loopback_ip_addr: &Ipv4Addr) {
    let rx_addr = SocketAddr::V4(SocketAddrV4::new(*loopback_ip_addr, 1350));
    let tx_addr = SocketAddr::V4(SocketAddrV4::new(*loopback_ip_addr, 1351));
    let rx = BluefinSocketRead::new(rx_addr);
    let tx = BluefinSocketRead::new(tx_addr);

    const MAX_DATA_SIZE: usize = 1024;
    let data_size = rng().random_range(1..=MAX_DATA_SIZE);
    assert!(data_size > 0 && data_size <= MAX_DATA_SIZE);

    let expected_data = Arc::new(generate_random_data(data_size));

    let cloned_expected_data = Arc::clone(&expected_data);
    let read_handle = tokio::spawn(async move {
        let mut buf = [0u8; MAX_DATA_SIZE];
        let mut input_buf = [IoSliceMut::new(&mut buf)];
        let mut metadata_buf = [BufMetadata::default()];
        let size = rx.recv(&mut input_buf, &mut metadata_buf).await;

        assert_eq!(size, data_size);
        assert_eq!(&buf[..size], &*cloned_expected_data);
        assert_eq!(metadata_buf[0].len, data_size);
        assert_eq!(metadata_buf[0].src_addr, tx_addr);
    });

    let transmit_data = TransmitData::new(tx_addr, rx_addr, &expected_data);
    let bytes_sent = tx.send_to(&transmit_data).expect("Could not send data");
    assert_eq!(bytes_sent, data_size);

    read_handle
        .await
        .expect("Could not read data from rx socket");
}

fn generate_random_data(size: usize) -> Vec<u8> {
    let mut ans = vec![];
    for _ in 0..size {
        ans.push(rng().random());
    }
    ans
}
