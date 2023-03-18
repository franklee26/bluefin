# Bluefin

`Bluefin` is an experimental, P2P, transport-layer protocol.

[![Latest Version]][crates.io] 
[![Documentation]][docs.rs]
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

### Example
#### Pack-leader
```rust
use bluefin::hosts::pack_leader::BluefinPackLeaderBuilder;

#[tokio::main]
async fn main() {
    // Bind and construct a pack-leader over an TUN device
    let mut pack_leader = BluefinPackLeader::builder()
        .name("utun3".to_string())
        .bind_address("192.168.55.2".to_string())
        .netmask("255.255.255.0".to_string())
        .source_id(0x01030108)
        .build();

    loop {
        let mut buf = vec![0; 1504];
        let connection_res = pack_leader.accept(&mut buf).await;

        tokio::spawn(async move {
            match connection_res {
                Ok(mut conn) => conn.process().await,
                Err(err) => eprintln!("\nConnection attempt failed: {:?}", err),
            }
        });
    }
}
```
#### Client
```rust
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .source_id(0x01020304)
        .build();

    let port = rand::thread_rng().gen_range(10000..50000);
    client
        .bind("0.0.0.0", port)
        .expect("Failed to bind client socket");
    let mut conn = client
        .connect("192.168.55.2", 31416)
        .await
        .expect("Failed to connect to host");

    Ok(())
}
```


[Latest Version]: https://img.shields.io/crates/v/bluefin.svg
[crates.io]: https://crates.io/crates/bluefin
[Documentation]: https://docs.rs/bluefin/badge.svg
[docs.rs]: https://docs.rs/bluefin