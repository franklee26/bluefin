use rand::Rng;

use bluefin::{
    core::{
        header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
        serialisable::Serialisable,
    },
    hosts::client::BluefinClient,
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .source_id([2, 5, 6, 7])
        .build();

    let port = rand::thread_rng().gen_range(10000..50000);
    client
        .bind("0.0.0.0", port)
        .expect("Failed to bind client socket");
    let mut conn = client
        .connect("192.168.55.2", 31416)
        .await
        .expect("Failed to connect to host");

    let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
    let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

    let mut header = BluefinHeader::new(*b"abcd", *b"efgh", type_fields, security_fields);
    header.with_packet_number([0x13, 0x18, 0x04, 0x20, 0xaa, 0xbb, 0xcc, 0xdd]);

    let bytes_out = header.serialise();
    conn.set_bytes_out(bytes_out);

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
