use bluefin::hosts::pack_leader::BluefinPackLeader;

#[tokio::main]
async fn main() {
    let mut pack_leader = BluefinPackLeader::builder()
        .name("utun8".to_string())
        .bind_address("192.168.55.2".to_string())
        .netmask("255.255.255.0".to_string())
        .build();

    loop {
        let conn_res = pack_leader.accept().await;
        tokio::spawn(async move {
            if let Ok(conn) = conn_res {
                eprintln!("Received: {conn}");
            }
        });
    }
}
