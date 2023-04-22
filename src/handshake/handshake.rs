use std::time::Duration;

use crate::{
    core::{
        context::{BluefinHost, State},
        error::BluefinError,
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::{BluefinPacket, Packet},
        serialisable::Serialisable,
    },
    io::Result,
    network::connection::Connection,
};

pub(crate) struct HandshakeHandler<'a> {
    conn: &'a mut Connection,
    host: BluefinHost,
    incoming_packet: Packet,
}

impl<'a> HandshakeHandler<'a> {
    pub(crate) fn new(
        conn: &'a mut Connection,
        incoming_packet: Packet,
        host: BluefinHost,
    ) -> Self {
        Self {
            conn,
            incoming_packet,
            host,
        }
    }

    pub(crate) async fn handle(&mut self) -> Result<()> {
        match self.host {
            BluefinHost::Client => self.bluefin_client_handshake_handler().await,
            BluefinHost::PackLeader => self.bluefin_packleader_handshake_handler().await,
            _ => todo!(),
        }
    }

    /// We have already parsed the client hello request. Now, the pack-leader needs to send it's
    /// pack-leader-hello back to the client.
    async fn bluefin_packleader_handshake_handler(&mut self) -> Result<()> {
        let header = self.incoming_packet.payload.header;

        // The source (client) is our destination. Connection id's can never be zero.
        let dest_id = header.source_connection_id;
        if dest_id == 0x0 {
            return Err(BluefinError::InvalidHeaderError(
                "Cannot have connection-id of zero".to_string(),
            ));
        }
        self.conn.dest_id = dest_id;

        // Set packet_number for context
        self.conn.context.packet_number = header.packet_number + 1;

        let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

        let mut header = BluefinHeader::new(
            self.conn.source_id,
            self.conn.dest_id,
            PacketType::UnencryptedHandshake,
            0x0,
            security_fields,
        );
        header.with_packet_number(self.conn.context.packet_number);

        // Build and send packet
        let packet = BluefinPacket::builder().header(header).build();

        if let Err(respond_err) = self.conn.write(&packet.serialise()).await {
            return Err(BluefinError::HandshakeError(format!(
                "Failed to send pack-leader response to client: {}",
                respond_err
            )));
        }

        // Read and validate client-ack. Let's keep this short so that we don't keep blocking the `Accept` thread
        if let Err(e) = self
            .conn
            .read_with_timeout_and_retries(Duration::from_secs(2), 2)
            .await
        {
            eprintln!("Failed to read client ack.");
            return Err(e);
        }
        // conn_read_result.packet.validate(conn)?;

        Ok(())
    }

    /// Pre-condition:
    ///     * Client has sent client-hello
    ///     * Pack-leader has sent it's response back to client
    ///
    /// This function parses the incoming pack-leader-hello and performs validation. If the incoming packet
    /// is valid then the connection context is updated and the client finally responds with its
    /// client-ack. As for the client, the handshake is established and the connection will go into
    /// ready state.
    async fn bluefin_client_handshake_handler(&mut self) -> Result<()> {
        // Just get the header; as usual, we don't care about the payload during this part of the handshake
        let header = self.incoming_packet.payload.header;

        // Verify that the connection id is correct (the packet's dst conn id should be our src id)
        if header.destination_connection_id != self.conn.source_id {
            return Err(BluefinError::InvalidHeaderError(format!(
                "Pack-leader's dst conn id ({}) != client's id ({})",
                header.destination_connection_id, self.conn.source_id
            )));
        }

        // Verify that the packet number is as expected
        if header.packet_number != self.conn.context.packet_number + 1 {
            return Err(BluefinError::InvalidHeaderError(format!(
                "Pack-leader has incorrect packet number {:#016x}, was expecting {:#016x}",
                header.packet_number, self.conn.context.packet_number
            )));
        }

        self.conn.context.packet_number += 2;

        // The source (pack-leader) is our destination. Conenction id's can never be zero.
        let dest_id = header.source_connection_id as u32;
        if dest_id == 0x0 {
            return Err(BluefinError::InvalidHeaderError(
                "Cannot have connection-id of zero".to_string(),
            ));
        }
        self.conn.dest_id = dest_id;
        // Validated the pack-leader's response. Ack-back.
        let ack_packet = self.conn.get_packet(None).serialise();

        if let Err(_) = self.conn.write(&ack_packet).await {
            return Err(BluefinError::HandshakeError(
                "Could not sent pack-leader the client-ack.".to_string(),
            ));
        }

        self.conn.context.state = State::Ready;

        Ok(())
    }
}
