use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use bluefin::{net::server::BluefinServer, utils::common::BluefinResult};
use tokio::{spawn, time::sleep};

#[tokio::main]
async fn main() -> BluefinResult<()> {
    let mut server = BluefinServer::new(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(192, 168, 1, 38),
        1318,
    )));
    server.bind().await?;

    const MAX_NUM_CONNECTIONS: usize = 5;
    for _ in 0..MAX_NUM_CONNECTIONS {
        let mut s = server.clone();
        let _ = spawn(async move {
            loop {
                println!();
                let _conn = s.accept().await;

                match _conn {
                    Ok(mut conn) => {
                        spawn(async move {
                            loop {
                                let mut recv_bytes = [0u8; 1024];
                                let size = conn.recv(&mut recv_bytes, 100).await.unwrap();

                                println!(
                                    "({:x}_{:x}) >>> Received: {:?}",
                                    conn.src_conn_id,
                                    conn.dst_conn_id,
                                    &recv_bytes[..size],
                                );
                                sleep(Duration::from_secs(1)).await;
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Could not accept connection due to error: {:?}", e);
                    }
                }

                sleep(Duration::from_secs(1)).await;
            }
        });
    }

    // The spawned tasks are looping forever. This infinite loop will keep the
    // process up forever.
    loop {}
}
