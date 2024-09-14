# Bluefin

`Bluefin` is an experimental, P2P, transport-layer protocol.

[![Latest Version]][crates.io] 
[![Documentation]][docs.rs]
![Github Workflow](https://github.com/franklee26/bluefin/actions/workflows/bluefin.yml/badge.svg)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

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

    const MAX_NUM_CONNECTIONS: usize = 5;
    for _ in 0..MAX_NUM_CONNECTIONS {
        let mut s = server.clone();
        let _ = spawn(async move {
            loop {
                let _conn = s.accept().await;

                match _conn {
                    Ok(mut conn) => {
                        spawn(async move {
                            loop {
                                let mut recv_bytes = [0u8; 1024];
                                let size = conn.recv(&mut recv_bytes, 100).await.unwrap();

                                println!(
                                    "({:x}_{:x}) >>> Received: {:?}",
                                    conn.src_conn_id,
                                    conn.dst_conn_id,
                                    &recv_bytes[..size],
                                );
                                sleep(Duration::from_secs(1)).await;
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Could not accept connection due to error: {:?}", e);
                    }
                }

                sleep(Duration::from_secs(1)).await;
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
    let task = spawn(async move {
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
        let mut size = conn.send(&bytes).await?;
        println!("Sent {} bytes", size);

        Ok::<(), BluefinError>(())
    });
    Ok(())
}
```


[Latest Version]: https://img.shields.io/crates/v/bluefin.svg
[crates.io]: https://crates.io/crates/bluefin
[Documentation]: https://docs.rs/bluefin/badge.svg
[docs.rs]: https://docs.rs/bluefin