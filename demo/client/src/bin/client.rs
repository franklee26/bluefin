use std::time::Duration;

use rand::Rng;

use bluefin::hosts::client::BluefinClient;
use tokio::time::sleep;

const NUMBER_OF_CONNECTIONS: usize = 5;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .timeout(Duration::from_secs(30))
        .build();

    let port = rand::thread_rng().gen_range(10000..50000);
    eprintln!("0.0.0.0:{}", port);
    client
        .bind("0.0.0.0", port)
        .expect("Failed to bind client socket");

    for i in 0..NUMBER_OF_CONNECTIONS {
        eprintln!();
        let other_port = rand::thread_rng().gen_range(10000..50000);
        let conn = client
            .connect("192.168.55.2", other_port)
            .await
            .expect("Failed to connect to host");
        tokio::spawn(async move {
            eprintln!("{i}: {conn}");
        });
        sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}
