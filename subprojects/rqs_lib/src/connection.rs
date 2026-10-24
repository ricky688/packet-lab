use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::{TcpSocket, TcpStream};
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// An explicit local route for one peer; all other peers retain system routing.
#[derive(Clone)]
pub(crate) struct Connector {
    route: Option<LanRoute>,
}

#[derive(Clone)]
struct LanRoute {
    peer: Ipv4Addr,
    interface: String,
}

impl LanRoute {
    fn parse(value: &str) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RQS_LAN_ROUTE must be a private IPv4 address and interface: 192.168.1.2%wlan0",
            )
        };
        let (peer, interface) = value.split_once('%').ok_or_else(invalid)?;
        let peer: Ipv4Addr = peer.parse().map_err(|_| invalid())?;
        if !peer.is_private()
            || interface.is_empty()
            || interface.len() > 15
            || !interface
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(invalid());
        }
        if !cfg!(target_os = "linux") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "RQS_LAN_ROUTE currently requires Linux",
            ));
        }
        Ok(Self {
            peer,
            interface: interface.into(),
        })
    }
}

impl Connector {
    pub(crate) fn interface(&self) -> Option<&str> {
        self.route.as_ref().map(|route| route.interface.as_str())
    }

    pub(crate) fn is_bluetooth_interface(interface: &str) -> bool {
        std::fs::canonicalize(format!("/sys/class/net/{interface}/device"))
            .ok()
            .is_some_and(|path| {
                path.components()
                    .any(|part| part.as_os_str() == "bluetooth")
            })
    }

    pub(crate) fn is_physical_lan_interface(name: &str) -> bool {
        if name.is_empty()
            || name.starts_with("p2p-dev-")
            || name.starts_with("vpn")
            || name.starts_with("tun")
            || name.starts_with("tap")
            || name.starts_with("wg")
            || name.starts_with("tailscale")
            || name.starts_with("docker")
            || name.starts_with("br-")
            || name.starts_with("veth")
            || name.starts_with("virbr")
            || name.starts_with("btnap")
            || name.starts_with("bnep")
            || name.starts_with("ap")
            || name == "lo"
        {
            return false;
        }
        let is_lan_prefix = name.starts_with("wl")
            || name.starts_with("en")
            || name.starts_with("eth")
            || name.starts_with("p2p-");
        is_lan_prefix && !Self::is_bluetooth_interface(name)
    }

    pub(crate) fn eligible_lan_interfaces(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(interfaces) = get_if_addrs::get_if_addrs() {
            for iface in interfaces {
                if let get_if_addrs::IfAddr::V4(v4) = iface.addr {
                    if v4.ip.is_private()
                        && Self::is_physical_lan_interface(&iface.name)
                        && !names.contains(&iface.name)
                    {
                        names.push(iface.name);
                    }
                }
            }
        }
        names
    }

    pub(crate) fn find_interface_for_peer(
        peer: Ipv4Addr,
        interfaces: &[get_if_addrs::Interface],
    ) -> Option<String> {
        if !peer.is_private() {
            return None;
        }
        let peer_u32 = u32::from(peer);
        for iface in interfaces {
            if !Self::is_physical_lan_interface(&iface.name) {
                continue;
            }
            if let get_if_addrs::IfAddr::V4(v4) = &iface.addr {
                if v4.ip.is_private() {
                    let mask = u32::from(v4.netmask);
                    let if_u32 = u32::from(v4.ip);
                    if mask != 0 && (peer_u32 & mask) == (if_u32 & mask) {
                        return Some(iface.name.clone());
                    }
                }
            }
        }
        None
    }

    pub(crate) fn is_bluetooth(&self) -> bool {
        self.interface()
            .is_some_and(Self::is_bluetooth_interface)
    }

    pub(crate) fn from_env() -> io::Result<Self> {
        let route = match std::env::var("RQS_LAN_ROUTE") {
            Ok(value) => Some(LanRoute::parse(&value)?),
            Err(std::env::VarError::NotPresent) => None,
            Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidInput, err)),
        };
        if let Some(route) = &route {
            info!(
                "Local sharing route: peer {} via {} (outside VPN tunnel)",
                route.peer, route.interface
            );
        }
        Ok(Self { route })
    }

    // TXT is unauthenticated: only accept the exact IP explicitly configured by the user.
    pub(crate) fn txt_candidate(&self, value: Option<&str>, port: u16) -> Option<SocketAddrV4> {
        let route = self.route.as_ref()?;
        let advertised: Ipv4Addr = value?.parse().ok()?;
        (advertised == route.peer).then(|| SocketAddrV4::new(route.peer, port))
    }

    pub(crate) async fn connect(&self, addr: &str) -> io::Result<TcpStream> {
        let attempt = async {
            if let (Some(route), Ok(SocketAddr::V4(peer))) = (&self.route, addr.parse()) {
                if *peer.ip() == route.peer {
                    let socket = TcpSocket::new_v4()?;
                    #[cfg(target_os = "linux")]
                    socket.bind_device(Some(route.interface.as_bytes()))?;
                    // Never retry this peer through a different route after binding fails.
                    let stream = socket.connect(SocketAddr::V4(peer)).await?;
                    debug!(
                        "Local sharing TCP: {} -> {} via {}",
                        stream.local_addr()?,
                        peer,
                        route.interface
                    );
                    return Ok(stream);
                }
            } else if let Ok(SocketAddr::V4(peer)) = addr.parse() {
                if let Ok(interfaces) = get_if_addrs::get_if_addrs() {
                    if let Some(iface) = Self::find_interface_for_peer(*peer.ip(), &interfaces) {
                        if let Ok(socket) = TcpSocket::new_v4() {
                            #[cfg(target_os = "linux")]
                            let bound = socket.bind_device(Some(iface.as_bytes())).is_ok();
                            #[cfg(not(target_os = "linux"))]
                            let bound = false;
                            if bound {
                                if let Ok(stream) = socket.connect(SocketAddr::V4(peer)).await {
                                    debug!(
                                        "Auto-bound local sharing TCP: {} -> {} via {}",
                                        stream.local_addr()?,
                                        peer,
                                        iface
                                    );
                                    return Ok(stream);
                                }
                            }
                        }
                    }
                }
            }
            TcpStream::connect(addr).await
        };
        timeout(CONNECT_TIMEOUT, attempt).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Quick Share connection to {addr} timed out"),
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn rejects_invalid_or_nonlocal_routes() {
        for value in [
            "",
            "wlo1",
            "8.8.8.8%wlan0",
            "127.0.0.1%lo",
            "10.0.0.2%",
            "10.0.0.2%bad/name",
            "10.0.0.2%this-name-is-too-long",
            "10.0.0.2%wlan0%vpn",
        ] {
            assert!(LanRoute::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn txt_address_must_match_explicit_peer() {
        let connector = Connector {
            route: Some(LanRoute::parse("10.0.0.2%wlan0").unwrap()),
        };
        assert_eq!(
            connector.txt_candidate(Some("10.0.0.2"), 54321),
            Some("10.0.0.2:54321".parse().unwrap())
        );
        for value in [None, Some("10.0.0.9"), Some("8.8.8.8"), Some("invalid")] {
            assert!(connector.txt_candidate(value, 54321).is_none());
        }
        assert!(
            Connector { route: None }
                .txt_candidate(Some("10.0.0.2"), 54321)
                .is_none()
        );
    }

    #[tokio::test]
    async fn normal_route_and_unrelated_peer_still_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let ordinary = Connector { route: None };
        ordinary.connect(&addr).await.unwrap();
        let other_peer = Connector {
            route: Some(LanRoute {
                peer: Ipv4Addr::new(10, 0, 0, 2),
                interface: "no-such-device".into(),
            }),
        };
        other_peer.connect(&addr).await.unwrap();
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn selected_route_binds_and_does_not_fall_back() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut connector = Connector {
            route: Some(LanRoute {
                peer: Ipv4Addr::LOCALHOST,
                interface: "lo".into(),
            }),
        };
        connector.connect(&addr).await.unwrap();
        connector.route.as_mut().unwrap().interface = "no-such-device".into();
        assert!(connector.connect(&addr).await.is_err());
    }

    #[test]
    fn filters_virtual_and_vpn_interfaces_and_matches_lan_subnet() {
        for physical in ["wlo1", "wlan0", "eth0", "enp3s0", "p2p-wlo1-0"] {
            assert!(
                Connector::is_physical_lan_interface(physical),
                "expected {physical} to be recognized as physical LAN"
            );
        }
        for virt in [
            "lo",
            "vpn",
            "tun0",
            "tailscale0",
            "docker0",
            "btnap0",
            "bnep0",
            "virbr0",
            "veth1234",
            "br-abcdef",
            "p2p-dev-wlo1",
            "ap0",
        ] {
            assert!(
                !Connector::is_physical_lan_interface(virt),
                "expected {virt} to be filtered out as virtual/non-LAN"
            );
        }

        let fake_ifs = vec![
            get_if_addrs::Interface {
                name: "vpn".into(),
                addr: get_if_addrs::IfAddr::V4(get_if_addrs::Ifv4Addr {
                    ip: Ipv4Addr::new(10, 14, 0, 2),
                    netmask: Ipv4Addr::new(255, 255, 0, 0),
                    broadcast: None,
                }),
            },
            get_if_addrs::Interface {
                name: "docker0".into(),
                addr: get_if_addrs::IfAddr::V4(get_if_addrs::Ifv4Addr {
                    ip: Ipv4Addr::new(172, 17, 0, 1),
                    netmask: Ipv4Addr::new(255, 255, 0, 0),
                    broadcast: None,
                }),
            },
            get_if_addrs::Interface {
                name: "wlo1".into(),
                addr: get_if_addrs::IfAddr::V4(get_if_addrs::Ifv4Addr {
                    ip: Ipv4Addr::new(192, 168, 0, 143),
                    netmask: Ipv4Addr::new(255, 255, 255, 0),
                    broadcast: Some(Ipv4Addr::new(192, 168, 0, 255)),
                }),
            },
            get_if_addrs::Interface {
                name: "p2p-wlo1-0".into(),
                addr: get_if_addrs::IfAddr::V4(get_if_addrs::Ifv4Addr {
                    ip: Ipv4Addr::new(192, 168, 49, 2),
                    netmask: Ipv4Addr::new(255, 255, 255, 0),
                    broadcast: Some(Ipv4Addr::new(192, 168, 49, 255)),
                }),
            },
        ];

        // Any random phone on 192.168.0.x auto-routes to wlo1
        assert_eq!(
            Connector::find_interface_for_peer(Ipv4Addr::new(192, 168, 0, 55), &fake_ifs),
            Some("wlo1".into())
        );
        assert_eq!(
            Connector::find_interface_for_peer(Ipv4Addr::new(192, 168, 0, 201), &fake_ifs),
            Some("wlo1".into())
        );
        // Any phone on Wi-Fi Direct 192.168.49.x auto-routes to p2p-wlo1-0
        assert_eq!(
            Connector::find_interface_for_peer(Ipv4Addr::new(192, 168, 49, 1), &fake_ifs),
            Some("p2p-wlo1-0".into())
        );
        // VPN and Docker subnets are never bound as physical LAN interfaces
        assert_eq!(
            Connector::find_interface_for_peer(Ipv4Addr::new(10, 14, 0, 99), &fake_ifs),
            None
        );
        assert_eq!(
            Connector::find_interface_for_peer(Ipv4Addr::new(172, 17, 0, 5), &fake_ifs),
            None
        );
    }
}
