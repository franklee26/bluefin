use bluefin::{
    core::context::{BluefinHost, Context, State},
    handshake::handshake::bluefin_handshake_handle,
};
use etherparse::PacketHeaders;
use tun_tap::{Iface, Mode};

const IPV4_PROTO: u16 = 0x0800;
const ICMP_PROTO: u8 = 0x01;
const UDP_PROTO: u8 = 0x11;
const TCP_PROTO: u8 = 0x06;

#[tokio::main]
async fn main() {
    let iface = Iface::new("frank_tun", Mode::Tun).expect("Failed to create a TUN device");
    let _name = iface.name();

    let mut buffer = vec![0; 1504]; // MTU + 4 for the header
    loop {
        let nbytes = iface.recv(&mut buffer).unwrap();

        // tuntap frames have 2 bytes for flags and 2 bytes for protocol iana
        assert!(nbytes >= 4);
        let flags = u16::from_be_bytes([buffer[0], buffer[1]]);
        let protocol = u16::from_be_bytes([buffer[2], buffer[3]]);

        // Only process ipv4 packets
        if protocol != IPV4_PROTO {
            continue;
        }

        // in the ipv4 header, the next-level protocol starts at the 72 bit offset
        // (9 bytes) and is 8 bits long
        let inner_protocol = buffer[13];

        // Only process datagrams using icmp, udp or tcp
        if inner_protocol != ICMP_PROTO
            && inner_protocol != UDP_PROTO
            && inner_protocol != TCP_PROTO
        {
            continue;
        }

        eprintln!("Inner protocol: {:#04x}", inner_protocol);

        let context = Context {
            host_type: BluefinHost::Server,
            state: State::Handshake,
        };

        match PacketHeaders::from_ip_slice(&buffer[4..nbytes]) {
            Err(value) => eprintln!("Encountered error parsing ipv4 datagram {:?}", value),
            Ok(value) => {
                eprintln!("link: {:?}", value.link);
                eprintln!("vlan: {:?}", value.vlan);
                eprintln!("ip: {:?}", value.ip);
                eprintln!("transport: {:?}", value.transport);
                let _res = bluefin_handshake_handle(&context, value.payload);
                eprintln!();
            }
        }
    }
}
