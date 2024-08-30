use etherparse::PacketBuilder;

use crate::core::error::BluefinError;

pub type BluefinResult<T> = Result<T, BluefinError>;

#[macro_export]
macro_rules! set_some_builder_field {
    ($field_name:ident,$field_type:ident) => {
        #[inline]
        pub fn $field_name(mut self, $field_name: $field_type) -> Self {
            self.$field_name = Some($field_name);
            self
        }
    };
}

/// Converts a string representation of an ip address to a byte-vector representation
#[inline]
pub(crate) fn string_to_vec_ip(ip: &str) -> Vec<u8> {
    ip.split(".").map(|s| s.parse::<u8>().unwrap()).collect()
}

/// Metadata context required to build write-able packets
#[derive(Debug)]
pub(crate) struct WriteContext {
    pub(crate) need_ip_udp_headers: bool,
    pub(crate) src_ip: [u8; 4],
    pub(crate) dst_ip: [u8; 4],
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
}

/// Returns writeable bytes to the wire by preprending any tuntap + ip + udp headers (if required)
#[inline]
pub(crate) fn get_writeable_bytes(data: &[u8], context: WriteContext) -> Vec<u8> {
    // No need to pre-build; just flush buffer
    if !context.need_ip_udp_headers {
        return data.into();
    }

    let packet_builder = PacketBuilder::ipv4(context.src_ip, context.dst_ip, 20)
        .udp(context.src_port, context.dst_port);

    let mut tun_tap_bytes = vec![0, 0, 0, 2];
    let mut writer = Vec::<u8>::with_capacity(packet_builder.size(data.len()));
    let _ = packet_builder.write(&mut writer, data);
    tun_tap_bytes.append(&mut writer);

    tun_tap_bytes
}
