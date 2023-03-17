use rand::Rng;

use bluefin::hosts::client::BluefinClient;

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

    let mut buf = vec![0; 1504];
    let size = conn.read(&mut buf).await?;
    eprintln!("{:?}", &buf[..size]);

    Ok(())
}
