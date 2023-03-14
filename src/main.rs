use bluefin::hosts::pack_leader::BluefinPackLeaderBuilder;

#[tokio::main]
async fn main() {
    let mut pack_leader = BluefinPackLeaderBuilder::builder()
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
