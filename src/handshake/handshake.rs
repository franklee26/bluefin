use std::io;

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

/// We have already parsed the client hello request. Now, the pack-leader needs to send it's
/// pack-leader-hello back to the client.
async fn bluefin_packleader_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
    let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

    // Build handshake header
    conn.context.packet_number += 1;
    let mut header = BluefinHeader::new(conn.source_id, conn.dest_id, type_fields, security_fields);
    header.with_packet_number(conn.context.packet_number);

    // Build and send packet
    let packet = BluefinPacket::builder().header(header).build();
    conn.set_bytes_out(packet.serialise());
    if let Err(respond_err) = conn.write().await {
        return Err(BluefinError::HandshakeError(format!(
            "Failed to send pack-leader response to client: {}",
            respond_err
        )));
    }

    Ok(())
}

async fn bluefin_packfollower_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}

fn handle_pack_leader_response(conn: &mut Connection) -> Result<(), BluefinError> {
    let packet = BluefinPacket::deserialise(conn.bytes_in.as_ref().unwrap())?;
    // Just get the header; as usual, we don't care about the payload during this part of the handshake
    let header = packet.header;

    // Verify that the connection id is correct (the packet's dst conn id should be our src id):w
    if header.destination_connection_id != conn.source_id {
        return Err(BluefinError::InvalidHeaderError(format!(
            "Pack-leader's dst conn id ({}) != client's id ({})",
            header.destination_connection_id, conn.source_id
        )));
    }

    // The source (pack-leader) is our destination. Conenction id's can never be zero.
    let dest_id = header.source_connection_id;
    if dest_id == 0x0 {
        return Err(BluefinError::InvalidHeaderError(
            "Cannot have connection-id of zero".to_string(),
        ));
    }
    conn.dest_id = dest_id;

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
    // Temporarily set dest id to zero
    let mut header = BluefinHeader::new(client_conn_id, 0x0, type_fields, security_fields);
    let packet_number: i64 = rand::thread_rng().gen_range(0..=i64::MAX);
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
            if size < 20 {
                return Err(BluefinError::HandshakeError(format!(
                    "Pack-leader responded with unexpected packet size of {} bytes",
                    size
                )));
            }
            handle_pack_leader_response(conn)?;
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
