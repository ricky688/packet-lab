use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::{TcpSocket, TcpStream};
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// An explicit local route for one peer; all other peers retain system routing.
pub(crate) struct Connector {
    route: Option<LanRoute>,
}

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
}
