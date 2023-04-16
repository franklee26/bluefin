use bluefin::{core::serialisable::Serialisable, hosts::pack_leader::BluefinPackLeader};

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
            }
        });
    }
}
