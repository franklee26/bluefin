# Bluefin

`Bluefin` is an experimental, P2P, transport-layer protocol.

[![Latest Version]][crates.io] 
[![Documentation]][docs.rs]
![Github Workflow](https://github.com/franklee26/bluefin/actions/workflows/bluefin.yml/badge.svg)
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
        .build();

    loop {
        let connection_res = pack_leader.accept().await;

        tokio::spawn(async move {
            match connection_res {
                Ok(conn) => println!("Conn ready! {conn})"),
                Err(err) => eprintln!("\nConnection attempt failed: {:?}", err),
            }
        });

        sleep(Duration::from_secs(1)).await;
    }
}
```
#### Client
```rust
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .build();

    let port = rand::thread_rng().gen_range(10000..50000);
    client
        .bind("0.0.0.0", port)
        .expect("Failed to bind client socket");
    let mut conn = client
        .connect("192.168.55.2", 31416)
        .await
        .expect("Failed to connect to host");

    eprintln!("Conn ready! {conn}");
    Ok(())
}
```


[Latest Version]: https://img.shields.io/crates/v/bluefin.svg
[crates.io]: https://crates.io/crates/bluefin
[Documentation]: https://docs.rs/bluefin/badge.svg
[docs.rs]: https://docs.rs/bluefin