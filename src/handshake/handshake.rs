use rand::Rng;

use crate::{
    connection::connection::Connection,
    core::{
        context::BluefinHost,
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
        packet::BluefinPacket,
        serialisable::Serialisable,
    },
};

pub async fn bluefin_handshake_handle(conn: &mut Connection) -> Result<(), BluefinError> {
    match conn.context.host_type {
        BluefinHost::Client => bluefin_client_handshake_handler(conn).await,
        BluefinHost::PackLeader => bluefin_packleader_handshake_handler(conn).await,
        BluefinHost::PackFollower => bluefin_packfollower_handshake_handler(conn).await,
    }
}

async fn bluefin_packleader_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);

    Ok(())
}

async fn bluefin_packfollower_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}

/*
 * Bluefin clients init the handshake. Therefore, we need to prepare the header and send it
 * to the pack_leader (or whatever we think is the current pack_leader).
 */
async fn bluefin_client_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
    let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

    // Build handshake header
    let client_conn_id = conn.source_id;
    let mut header = BluefinHeader::new(client_conn_id, *b"efgh", type_fields, security_fields);
    let mut packet_number: [u8; 8] = [0; 8];
    for i in 0..8 {
        let byte = rand::thread_rng().gen_range(0..=255);
        packet_number[i] = byte;
    }
    header.with_packet_number(packet_number);

    // Build and send packet
    let packet = BluefinPacket::builder().header(header).build();
    conn.set_bytes_out(packet.serialise());

    if let Err(send_err) = conn.write().await {
        return Err(BluefinError::HandshakeError(format!(
            "Failed to send client-hello {:?}",
            send_err
        )));
    };

    // Wait for pack-leader response (20 bytes)
    let mut buf = vec![0; 20];
    match conn.read(&mut buf).await {
        Ok(size) => {
            eprintln!("Received: {:?}", &buf[..size]);
        }
        Err(err) => {
            return Err(BluefinError::HandshakeError(format!(
                "Failed to read server-hello response {:?}",
                err
            )));
        }
    };

    Ok(())
}
