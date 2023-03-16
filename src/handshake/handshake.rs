use crate::{
    connection::connection::Connection,
    core::{
        context::BluefinHost, error::BluefinError, header::BluefinHeader,
        serialisable::Serialisable,
    },
};

pub fn bluefin_handshake_handle(conn: &mut Connection) -> Result<(), BluefinError> {
    match conn.context.host_type {
        BluefinHost::Client => bluefin_client_handshake_handler(conn),
        BluefinHost::PackLeader => bluefin_packleader_handshake_handler(conn),
        BluefinHost::PackFollower => bluefin_packfollower_handshake_handler(conn),
    }
}

fn bluefin_packleader_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);

    Ok(())
}

fn bluefin_packfollower_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}

fn bluefin_client_handshake_handler(conn: &mut Connection) -> Result<(), BluefinError> {
    let deserialised = BluefinHeader::deserialise(&conn.bytes_in.as_ref().unwrap())?;
    eprintln!("Found header: {:?}", deserialised);
    Ok(())
}
