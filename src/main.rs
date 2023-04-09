use bluefin::hosts::pack_leader::BluefinPackLeader;

#[tokio::main]
async fn main() {
    let mut pack_leader = BluefinPackLeader::builder()
        .name("utun7".to_string())
        .bind_address("192.168.55.2".to_string())
        .netmask("255.255.255.0".to_string())
        .source_id(0x01030108)
        .build();

    loop {
        eprintln!("Loop begins");
        let conn_res = pack_leader.accept().await;
        eprintln!(">>> Accepted!");
        tokio::spawn(async move {
            if let Ok(conn) = conn_res {}
            // while let Ok(mut stream) = conn.accept_stream().await {
            //     tokio::spawn(async move {
            //         eprintln!("Stream: {}", stream.id);
            //         stream.receive().await;
            //     });
            // }
        });
    }
}
