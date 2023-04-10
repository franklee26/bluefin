use std::{sync::Arc, time::Duration};

use rand::Rng;
use tokio::time::timeout;

use crate::{
    core::{
        context::BluefinHost,
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
        packet::BluefinPacket,
        serialisable::Serialisable,
    },
    io::read::Read,
    network::connection::Connection,
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
        eprintln!("Failed to write...");
        return Err(BluefinError::HandshakeError(format!(
            "Failed to send pack-leader response to client: {}",
            respond_err
        )));
    }

    eprintln!("Reading client ack...");
    // Read and validate client-ack
    let packet = conn.read_v2().await?;
    // conn_read_result.packet.validate(conn)?;

    Ok(())
}

async fn bluefin_packfollower_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    todo!();
}

/// Pre-condition:
///     * Client has sent client-hello
///     * Pack-leader has sent it's response back to client
///
/// This function parses the incoming pack-leader-hello and performs validation. This is
/// almost identical to the `validate` function in `Packet.rs` except we need to set
/// connection's destination id (we did not know this value yet until the pack-leader has
/// responded back with it's pack-leader-hello).
async fn handle_pack_leader_response(
    conn: &mut Connection,
    packet: BluefinPacket,
) -> Result<(), BluefinError> {
    // Just get the header; as usual, we don't care about the payload during this part of the handshake
    let header = packet.header;

    // Verify that the connection id is correct (the packet's dst conn id should be our src id)
    if header.destination_connection_id != conn.source_id {
        return Err(BluefinError::InvalidHeaderError(format!(
            "Pack-leader's dst conn id ({}) != client's id ({})",
            header.destination_connection_id, conn.source_id
        )));
    }

    // Verify that the packet number is as expected
    if header.packet_number != conn.context.packet_number + 1 {
        return Err(BluefinError::InvalidHeaderError(format!(
            "Pack-leader has incorrect packet number {:#016x}, was expecting {:#016x}",
            header.packet_number, conn.context.packet_number
        )));
    }

    conn.context.packet_number += 2;

    // The source (pack-leader) is our destination. Conenction id's can never be zero.
    let dest_id = header.source_connection_id as u32;
    if dest_id == 0x0 {
        return Err(BluefinError::InvalidHeaderError(
            "Cannot have connection-id of zero".to_string(),
        ));
    }
    conn.dest_id = dest_id;
    // Validated the pack-leader's response. Ack-back.
    let ack_packet = conn.get_packet(None).serialise();
    conn.set_bytes_out(ack_packet);
    if let Err(_) = conn.write().await {
        return Err(BluefinError::HandshakeError(
            "Could not sent pack-leader the client-ack.".to_string(),
        ));
    }
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
    conn.context.packet_number = packet_number;

    // Register new connection
    let first_read_key = format!("0_{}", client_conn_id);
    // Lock acquired
    {
        let mut manager = (*conn.conn_manager).lock().unwrap();
        manager.register_new_connection(first_read_key.clone());
    }
    // Lock released

    // Build and send client-hello
    let packet = BluefinPacket::builder().header(header).build();
    conn.set_bytes_out(packet.serialise());

    if let Err(send_err) = conn.write().await {
        return Err(BluefinError::HandshakeError(format!(
            "Failed to send client-hello {:?}",
            send_err
        )));
    };

    // Wait for pack-leader response. We should be more generous with the timeout here as the pack-leader
    // might be busy.
    let packet = conn
        .read_with_id_and_timeout(first_read_key.clone(), Duration::from_secs(10))
        .await?;
    let key = format!(
        "{}_{}",
        packet.payload.header.source_connection_id, packet.payload.header.destination_connection_id
    );
    // Lock acquired.
    {
        let mut manager = (*conn.conn_manager).lock().unwrap();
        // Remove temp key
        let _ = manager.remove_conn(first_read_key.clone());
        // Register new connection buffer
        manager.register_new_connection(key);
    }
    // Lock released

    eprintln!("Read: {:?}", packet);
    // handle_pack_leader_response(conn, conn_read_result.packet).await?;

    Ok(())
}
