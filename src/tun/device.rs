use std::net::Ipv4Addr;

use std::os::fd::IntoRawFd;
use std::process::Command;
use tun::platform::Device;
use tun_tap::Iface;

// This only exists because it's a pain to work with tun/tap devices
// on Mac OS
pub struct BluefinDevice {
    name: Option<String>,
    address: Option<Ipv4Addr>,
    netmask: Option<Ipv4Addr>,
    mac_device: Option<Device>,
    linux_device: Option<Iface>,
}

pub struct BluefinDeviceBuilder {
    name: Option<String>,
    address: Option<Ipv4Addr>,
    netmask: Option<Ipv4Addr>,
}

impl BluefinDeviceBuilder {
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn address(mut self, address: Ipv4Addr) -> Self {
        self.address = Some(address);
        self
    }

    pub fn netmask(mut self, netmask: Ipv4Addr) -> Self {
        self.netmask = Some(netmask);
        self
    }

    #[cfg(target_os = "macos")]
    fn get_mac_device(self) -> Option<Device> {
        let mut config = tun::Configuration::default();
        config
            .layer(tun::Layer::L3)
            .address(self.address.unwrap())
            .netmask(self.netmask.unwrap())
            .up();

        let dev = tun::create(&config).unwrap();

        Some(dev)
    }

    #[cfg(target_os = "linux")]
    fn get_linux_device(self) -> Option<Iface> {
        let name = self.name.clone().unwrap();
        let iface = Iface::new(name, Mode::Tun).expect("Failed to create a TUN device");
        Some(iface)
    }

    pub fn build(self) -> BluefinDevice {
        #[cfg(target_os = "macos")]
        let device = {
            let name = self.name.clone().unwrap();
            let address = self.address.clone().unwrap();
            let netmask = self.netmask;
            let mac_device = self.get_mac_device();

            let device = BluefinDevice {
                name: Some(name.clone()),
                address: Some(address),
                netmask,
                mac_device,
                linux_device: None,
            };

            let up_eth_str: String = format!("ifconfig {} {:?} {:?} up ", name, address, address);
            let route_add_str: String = format!(
                "sudo route -n add -net {:?} -netmask {:?} {:?}",
                address,
                device.netmask.unwrap(),
                address
            );
            let up_eth_out = Command::new("sh")
                .arg("-c")
                .arg(up_eth_str)
                .output()
                .expect("sh exec error!");
            if !up_eth_out.status.success() {
                eprintln!("Failed first command!!!");
            }

            let if_config_out = Command::new("sh")
                .arg("-c")
                .arg(route_add_str)
                .output()
                .expect("sh exec error!");
            if !if_config_out.status.success() {
                eprintln!("Failed second command!!!");
            }

            device
        };

        #[cfg(target_os = "linux")]
        let device = {
            BluefinDevice {
                name: self.name.to_owned(),
                address: self.address,
                netmask: self.netmask,
                mac_device: None,
                linux_device: self.get_linux_device(),
                source_ip: None,
                destination_ip: None,
                source_port: None,
                destination_port: None,
                fd: None,
            }
        };

        device
    }
}

impl BluefinDevice {
    pub fn builder() -> BluefinDeviceBuilder {
        BluefinDeviceBuilder {
            name: None,
            address: None,
            netmask: None,
        }
    }

    pub fn get_raw_fd(self) -> i32 {
        #[cfg(target_os = "macos")]
        let raw_fd = {
            let netmask = self.netmask;
            let address = self.address.unwrap();
            let name = self.name.clone().unwrap();
            let mac_device = self.mac_device.unwrap();
            let raw_fd = mac_device.into_raw_fd();

            let up_eth_str: String = format!("ifconfig {} {:?} {:?} up ", name, address, address);
            let route_add_str: String = format!(
                "sudo route -n add -net {:?} -netmask {:?} {:?}",
                address,
                netmask.unwrap(),
                address
            );
            let up_eth_out = Command::new("sh")
                .arg("-c")
                .arg(up_eth_str)
                .output()
                .expect("sh exec error!");
            if !up_eth_out.status.success() {
                eprintln!("Failed first command!!!");
            }

            let if_config_out = Command::new("sh")
                .arg("-c")
                .arg(route_add_str)
                .output()
                .expect("sh exec error!");
            if !if_config_out.status.success() {
                eprintln!("Failed second command!!!");
            }

            raw_fd
        };

        #[cfg(target_os = "linux")]
        let raw_fd = {
            let linux_device = self.linux_device.unwrap();
            linux_device.into_raw_fd()
        };

        raw_fd
    }
}
