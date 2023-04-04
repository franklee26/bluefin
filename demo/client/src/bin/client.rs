use std::time::Duration;

use rand::Rng;

use bluefin::hosts::client::BluefinClient;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .timeout(Duration::from_secs(30))
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

    // for _ in 0..5 {
    //     let stream = conn.request_stream().await.unwrap();
    //     tokio::spawn(async move {
    //         eprintln!("Opened stream: {}", stream.id);
    //     });
    // }

    let packet = conn.bluefin_read_packet().await.unwrap().packet;
    eprintln!(
        "<{:?}> Received payload: {}",
        packet.header.type_and_type_specific_payload.packet_type,
        std::str::from_utf8(&packet.payload).unwrap()
    );
    eprintln!("{conn}");

    Ok(())
}
