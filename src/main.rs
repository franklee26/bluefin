use std::time::Duration;

use bluefin::{core::serialisable::Serialisable, hosts::pack_leader::BluefinPackLeader};
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    let mut pack_leader = BluefinPackLeader::builder()
        .name("utun3".to_string())
        .bind_address("192.168.55.2".to_string())
        .netmask("255.255.255.0".to_string())
        .build();

    loop {
        let conn_res = pack_leader.accept().await;
        tokio::spawn(async move {
            if let Ok(mut conn) = conn_res {
                eprintln!("Received: {conn}");
                let payload = format!("Hello {:#08x}, from {:#08x}!", conn.dest_id, conn.source_id);
                let packet = conn.get_packet(Some(payload.as_bytes().into()));

                let _ = conn.write(&packet.serialise()).await;

                sleep(Duration::from_millis(1250)).await;

                let payload = format!("This is my second message to {:#08x}...", conn.dest_id);
                let packet = conn.get_packet(Some(payload.as_bytes().into()));

                let _ = conn.write(&packet.serialise()).await;
            }
        });
    }
}
