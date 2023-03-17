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
        .source_id([1, 3, 1, 8])
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
    // Construct bluefin client
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .source_id([2, 5, 6, 7])
        .build();

    // bind and connect socket
    let port = rand::thread_rng().gen_range(10000..50000);
    client
        .bind("0.0.0.0", port)
        .expect("Failed to bind client socket");
    let mut conn = client
        .connect("192.168.55.2", 31416)
        .await
        .expect("Failed to connect to host");

    // build arbitrary bluefin packet
    let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
    let security_fields = BluefinSecurityFields::new(true, 0b000_1111);
    let mut header = BluefinHeader::new(*b"abcd", *b"efgh", type_fields, security_fields);
    header.with_packet_number([0x13, 0x18, 0x04, 0x20, 0xaa, 0xbb, 0xcc, 0xdd]);
    let bytes_out = header.serialise();
    conn.set_bytes_out(bytes_out);

    // send packet
    let written = conn.write().await?;
    eprintln!("Wrote: {} byte(s)", written);

    eprintln!("Waiting for response...");
    let mut buffer = vec![0; 1504];
    match conn.read(&mut buffer).await {
        Ok(received) => eprintln!("Received response: {:?}", &buffer[..received]),
        Err(_) => eprintln!("Error reading response"),
    };
    Ok(())
}

```


[Latest Version]: https://img.shields.io/crates/v/bluefin.svg
[crates.io]: https://crates.io/crates/bluefin
[Documentation]: https://docs.rs/bluefin/badge.svg
[docs.rs]: https://docs.rs/bluefin