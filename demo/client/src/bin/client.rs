use std::time::Duration;

use rand::Rng;

use bluefin::hosts::client::BluefinClient;
use tokio::time::sleep;

const NUMBER_OF_CONNECTIONS: usize = 50;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut client = BluefinClient::builder()
        .name("test_client".to_string())
        .timeout(Duration::from_secs(30))
        .build();

    let port = rand::thread_rng().gen_range(10000..50000);
    let other_port = rand::thread_rng().gen_range(10000..50000);
    eprintln!("0.0.0.0:{}", port);
    let _ = client.bind("0.0.0.0", port);

    for i in 0..NUMBER_OF_CONNECTIONS {
        eprintln!();
        let conn_res = client.connect("192.168.55.2", other_port).await;
        tokio::spawn(async move {
            match conn_res {
                Ok(conn) => {
                    eprintln!("{i}: {conn}");
                }
                Err(e) => eprintln!("{i}: {e}"),
            }
        });
        sleep(Duration::from_millis(250)).await;
    }

    Ok(())
}
