use std::net::UdpSocket;

use bluefin::core::{
    header::{BluefinHeader, BluefinSecurityFields, BluefinTypeFields, PacketType},
    serialisable::Serialisable,
};

fn main() -> std::io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:34254")?;
    socket
        .connect("192.168.0.3:31416")
        .expect("connect function failed");

    let type_fields = BluefinTypeFields::new(PacketType::UnencryptedHandshake, 0x0);
    let security_fields = BluefinSecurityFields::new(true, 0b000_1111);

    let mut header = BluefinHeader::new(*b"abcd", *b"efgh", type_fields, security_fields);
    header.with_packet_number([0x13, 0x18, 0x04, 0x20, 0xaa, 0xbb, 0xcc, 0xdd]);

    socket.send(&header.serialise())?;
    Ok(())
}
