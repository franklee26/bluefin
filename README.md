# Bluefin

`Bluefin` is an experimental, P2P, transport-layer protocol. Unlike TCP, `Bluefin` runs in user-space and can allow for
faster development cycles, greater flexibility in new features and more performant resource management compared to
kernel-space implementations.
`Bluefin` is currently only supported on MacOs and Linux.

[![Latest Version]][crates.io]
[![Documentation]][docs.rs]
![Github Workflow](https://github.com/franklee26/bluefin/actions/workflows/bluefin.yml/badge.svg)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![codecov](https://codecov.io/github/franklee26/bluefin/graph/badge.svg?token=U0XPUZVE0I)](https://codecov.io/github/franklee26/bluefin)

### Example

#### Pack-leader

```rust
#[tokio::main]
async fn main() -> BluefinResult<()> {
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(192, 168, 1, 38),
        1235,
    )));
    server.bind().await?;

    while let Ok(conn) = s.accept().await {
        let _ = spawn(async move {
            let mut recv_bytes = [0u8; 1024];
            loop {
                let size = conn.recv(&mut recv_bytes, 1024).await.unwrap();

                println!(
                    "({:x}_{:x}) >>> Received: {:?}",
                    conn.src_conn_id,
                    conn.dst_conn_id,
                    &recv_bytes[..size],
                );
            }
        });
    }

    // The spawned tasks are looping forever. This infinite loop will keep the
    // process up forever.
    loop {}
}
```

#### Client

```rust
#[tokio::main]
async fn main() -> BluefinResult<()> {
    let mut client = BluefinClient::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(192, 168, 1, 38),
        1234,
    )));
    let mut conn = client
        .connect(std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(192, 168, 1, 38),
            1235,
        )))
        .await?;

    let bytes = [1, 2, 3, 4];
    let size = conn.send(&bytes);
    println!("Sent {} bytes", size);

    Ok(())
}
```

[Latest Version]: https://img.shields.io/crates/v/bluefin.svg

[crates.io]: https://crates.io/crates/bluefin

[Documentation]: https://docs.rs/bluefin/badge.svg

[docs.rs]: https://docs.rs/bluefin