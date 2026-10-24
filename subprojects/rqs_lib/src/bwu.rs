//! Nearby channel handover. UKEY2 keys and sequence numbers stay in the session.
use crate::{
    connection::Connector,
    location_nearby_connections::{
        self as wire,
        bandwidth_upgrade_negotiation_frame::{
            self as bwu, EventType,
            upgrade_path_info::{Medium, WifiLanSocket},
        },
    },
    transport::Transport,
};
use anyhow::{Context as _, anyhow, bail};
use prost::Message;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
    time::timeout,
};

const DEADLINE: Duration = Duration::from_secs(10);
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Transport>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pUpgradeRequest {
    pub medium: Medium,
    pub ssid: String,
    pub password: String,
    pub gateway: Ipv4Addr,
    pub port: u16,
    pub frequency: Option<i32>,
}

pub type P2pUpgrader = Arc<
    dyn Fn(
            P2pUpgradeRequest,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

static P2P_UPGRADER: std::sync::RwLock<Option<P2pUpgrader>> = std::sync::RwLock::new(None);

pub fn set_p2p_upgrader(upgrader: P2pUpgrader) {
    if let Ok(mut guard) = P2P_UPGRADER.write() {
        *guard = Some(upgrader);
    }
}

pub(crate) fn parse_p2p_upgrade_request(
    path: &bwu::UpgradePathInfo,
) -> anyhow::Result<P2pUpgradeRequest> {
    match path.medium() {
        Medium::WifiDirect => {
            let creds = path
                .wifi_direct_credentials
                .as_ref()
                .context("Missing Wi-Fi Direct credentials")?;
            let ssid = creds.ssid.as_deref().unwrap_or("").trim().to_owned();
            let password = creds.password.as_deref().unwrap_or("").to_owned();
            if ssid.is_empty() || ssid.len() > 32 || password.is_empty() {
                bail!("Invalid Wi-Fi Direct SSID or password");
            }
            let port = creds.port.unwrap_or(0);
            if port <= 0 || port > 65535 {
                bail!("Invalid Wi-Fi Direct port");
            }
            let gw_str = creds.gateway.as_deref().unwrap_or("0.0.0.0").trim();
            let gateway = if gw_str.is_empty() || gw_str == "0.0.0.0" {
                Ipv4Addr::new(192, 168, 49, 1)
            } else {
                gw_str.parse::<Ipv4Addr>().context("Invalid Wi-Fi Direct gateway")?
            };
            if !gateway.is_private() {
                bail!("Non-private Wi-Fi Direct gateway");
            }
            Ok(P2pUpgradeRequest {
                medium: Medium::WifiDirect,
                ssid,
                password,
                gateway,
                port: port as u16,
                frequency: creds.frequency,
            })
        }
        Medium::WifiHotspot => {
            let creds = path
                .wifi_hotspot_credentials
                .as_ref()
                .context("Missing Wi-Fi Hotspot credentials")?;
            let ssid = creds.ssid.as_deref().unwrap_or("").trim().to_owned();
            let password = creds.password.as_deref().unwrap_or("").to_owned();
            if ssid.is_empty() || ssid.len() > 32 || password.is_empty() {
                bail!("Invalid Wi-Fi Hotspot SSID or password");
            }
            let port = creds.port.unwrap_or(0);
            if port <= 0 || port > 65535 {
                bail!("Invalid Wi-Fi Hotspot port");
            }
            let gw_str = creds.gateway.as_deref().unwrap_or("0.0.0.0").trim();
            let gateway = if gw_str.is_empty() || gw_str == "0.0.0.0" {
                Ipv4Addr::new(192, 168, 43, 1)
            } else {
                gw_str.parse::<Ipv4Addr>().context("Invalid Wi-Fi Hotspot gateway")?
            };
            if !gateway.is_private() {
                bail!("Non-private Wi-Fi Hotspot gateway");
            }
            Ok(P2pUpgradeRequest {
                medium: Medium::WifiHotspot,
                ssid,
                password,
                gateway,
                port: port as u16,
                frequency: creds.frequency,
            })
        }
        other => bail!("Unsupported P2P medium {:?}", other),
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub endpoint: String,
    pub address: Option<SocketAddrV4>,
    pending: Pending,
}

impl Context {
    pub fn new(endpoint: [u8; 4], port: u16) -> Self {
        let interface = std::env::var("RQS_LAN_ROUTE")
            .ok()
            .and_then(|r| r.split_once('%').map(|(_, i)| i.to_owned()));
        let bluetooth_only = Connector::from_env()
            .ok()
            .is_some_and(|connector| connector.is_bluetooth());
        let address = get_if_addrs::get_if_addrs()
            .ok()
            .filter(|_| !bluetooth_only)
            .and_then(|interfaces| {
                interfaces.into_iter().find_map(|i| {
                    let eligible = interface.as_ref().map_or_else(
                        || Connector::is_physical_lan_interface(&i.name),
                        |name| *name == i.name,
                    );
                    match i.ip() {
                        std::net::IpAddr::V4(ip) if eligible && ip.is_private() => {
                            Some(SocketAddrV4::new(ip, port))
                        }
                        _ => None,
                    }
                })
            });
        Self {
            endpoint: String::from_utf8_lossy(&endpoint).into(),
            address,
            pending: Arc::default(),
        }
    }

    pub async fn route_introduction(
        &self,
        frame: &wire::OfflineFrame,
        stream: &mut Transport,
    ) -> anyhow::Result<bool> {
        let Some(v1) = &frame.v1 else {
            return Ok(false);
        };
        if v1.r#type() != wire::v1_frame::FrameType::BandwidthUpgradeNegotiation {
            return Ok(false);
        }
        let packet = v1
            .bandwidth_upgrade_negotiation
            .as_ref()
            .context("Missing upgrade frame")?;
        if packet.event_type() != EventType::ClientIntroduction {
            return Ok(false);
        }
        let intro = packet
            .client_introduction
            .as_ref()
            .context("Missing upgrade introduction")?;
        if intro.supports_disabling_encryption() {
            bail!("Unencrypted channel upgrades are unsupported");
        }
        if intro.endpoint_id().len() != 4
            || !intro
                .endpoint_id()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric())
        {
            bail!("Invalid upgrade endpoint ID");
        }
        let reply = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(intro.endpoint_id())
        }
        .context("Unsolicited upgrade connection")?;
        write_plain(stream, &event(EventType::ClientIntroductionAck)).await?;
        let socket = std::mem::replace(stream, Transport::new(tokio::io::empty()));
        reply
            .send(socket)
            .map_err(|_| anyhow!("Upgrade session already closed"))?;
        Ok(true)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Upgrade {
    pub context: Option<Context>,
    pub remote: Option<String>,
    pub bluetooth: bool,
    pub lan_failed: bool,
    next: Option<Transport>,
    waiting: Option<oneshot::Receiver<Transport>>,
    pub last_sent: bool,
    pub deferred: Vec<wire::OfflineFrame>,
    last_received: bool,
    pub complete: bool,
    pub deadline: Option<tokio::time::Instant>,
}

impl Drop for Upgrade {
    fn drop(&mut self) {
        if self.waiting.is_some() {
            if let Some(context) = &self.context {
                context
                    .pending
                    .lock()
                    .unwrap()
                    .remove(self.remote.as_deref().unwrap_or(""));
            }
        }
    }
}

pub(crate) enum Action {
    Send(Vec<wire::OfflineFrame>),
    Switch,
}

impl Upgrade {
    fn retain_bluetooth(&mut self, path: &bwu::UpgradePathInfo) -> Action {
        self.lan_failed = true;
        self.deadline = None;
        let mut failure = event(EventType::UpgradeFailure);
        failure
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .upgrade_path_info = Some(path.clone());
        Action::Send(vec![failure])
    }

    pub fn offer(&mut self) -> anyhow::Result<Option<wire::OfflineFrame>> {
        if !self.bluetooth || self.complete || self.waiting.is_some() || self.next.is_some() {
            return Ok(None);
        }
        let context = self.context.as_ref().context("Missing upgrade context")?;
        let Some(address) = context.address else {
            return Ok(None);
        };
        let remote = self
            .remote
            .as_deref()
            .context("Missing remote endpoint ID")?;
        let (tx, rx) = oneshot::channel();
        let mut pending = context.pending.lock().unwrap();
        if pending.contains_key(remote) {
            bail!("Endpoint already upgrading");
        }
        pending.insert(remote.to_owned(), tx);
        self.waiting = Some(rx);
        self.deadline = Some(tokio::time::Instant::now() + DEADLINE);
        let mut frame = event(EventType::UpgradePathAvailable);
        frame
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .upgrade_path_info = Some(bwu::UpgradePathInfo {
            medium: Some(Medium::WifiLan.into()),
            wifi_lan_socket: Some(WifiLanSocket {
                ip_address: Some(address.ip().octets().to_vec()),
                wifi_port: Some(i32::from(address.port())),
            }),
            supports_disabling_encryption: Some(false),
            supports_client_introduction_ack: Some(true),
            ..Default::default()
        });
        info!("Bluetooth -> Wi-Fi: offering local LAN endpoint {address}");
        Ok(Some(frame))
    }

    pub async fn handle(
        &mut self,
        packet: &wire::BandwidthUpgradeNegotiationFrame,
    ) -> anyhow::Result<Action> {
        if !self.bluetooth || self.complete {
            bail!("Unexpected bandwidth upgrade on this medium");
        }
        match packet.event_type() {
            EventType::UpgradePathAvailable => {
                if self.waiting.is_some() || self.next.is_some() {
                    bail!("Simultaneous/duplicate channel upgrades are unsupported");
                }
                self.deadline = Some(tokio::time::Instant::now() + DEADLINE);
                let path = packet
                    .upgrade_path_info
                    .as_ref()
                    .context("Missing upgrade path")?;
                if path.supports_disabling_encryption() {
                    warn!(
                        "Rejecting unencrypted upgrade path: medium={:?}",
                        path.medium()
                    );
                    let mut failure = event(EventType::UpgradeFailure);
                    failure
                        .v1
                        .as_mut()
                        .unwrap()
                        .bandwidth_upgrade_negotiation
                        .as_mut()
                        .unwrap()
                        .upgrade_path_info = Some(path.clone());
                    return Ok(Action::Send(vec![failure]));
                }
                let address = match path.medium() {
                    Medium::WifiLan => lan_address(
                        path.wifi_lan_socket
                            .as_ref()
                            .context("Missing Wi-Fi LAN socket")?,
                    )?,
                    Medium::WifiDirect | Medium::WifiHotspot => {
                        let req = match parse_p2p_upgrade_request(path) {
                            Ok(req) => req,
                            Err(error) => {
                                warn!(
                                    "Invalid {:?} upgrade credentials: {error:#}; retaining encrypted Bluetooth transport",
                                    path.medium()
                                );
                                return Ok(self.retain_bluetooth(path));
                            }
                        };
                        info!(
                            "Received {:?} upgrade offer: ssid={}, gateway={}, port={}, freq={:?}",
                            req.medium, req.ssid, req.gateway, req.port, req.frequency
                        );
                        let upgrader = P2P_UPGRADER.read().ok().and_then(|g| g.clone());
                        if let Some(upgrader) = upgrader {
                            if let Err(error) = upgrader(req.clone()).await {
                                warn!("Wi-Fi P2P link preparation returned warning: {error:#}");
                            }
                        }
                        SocketAddrV4::new(req.gateway, req.port)
                    }
                    _ => {
                        warn!(
                            "Rejecting unsupported upgrade medium: medium={:?}, path={:?}",
                            path.medium(),
                            path
                        );
                        let mut failure = event(EventType::UpgradeFailure);
                        failure
                            .v1
                            .as_mut()
                            .unwrap()
                            .bandwidth_upgrade_negotiation
                            .as_mut()
                            .unwrap()
                            .upgrade_path_info = Some(path.clone());
                        return Ok(Action::Send(vec![failure]));
                    }
                };
                let connection = Connector::from_env()?.connect(&address.to_string()).await;
                let mut socket = match connection {
                    Ok(socket) => Transport::new(socket),
                    Err(error) => {
                        warn!(
                            "Wi-Fi handover to {address} failed; retaining encrypted Bluetooth transport: {error:#}"
                        );
                        return Ok(self.retain_bluetooth(path));
                    }
                };
                let mut intro = event(EventType::ClientIntroduction);
                intro
                    .v1
                    .as_mut()
                    .unwrap()
                    .bandwidth_upgrade_negotiation
                    .as_mut()
                    .unwrap()
                    .client_introduction = Some(bwu::ClientIntroduction {
                    endpoint_id: Some(
                        self.context
                            .as_ref()
                            .context("Missing local endpoint")?
                            .endpoint
                            .clone(),
                    ),
                    supports_disabling_encryption: Some(false),
                });
                write_plain(&mut socket, &intro).await?;
                if path.supports_client_introduction_ack() {
                    let ack = read_plain(&mut socket).await?;
                    let ack = ack
                        .v1
                        .and_then(|v| v.bandwidth_upgrade_negotiation)
                        .context("Missing upgrade ACK")?;
                    if ack.event_type() != EventType::ClientIntroductionAck {
                        bail!("Invalid upgrade ACK");
                    }
                }
                self.next = Some(socket);
                self.last_sent = true;
                Ok(Action::Send(vec![event(
                    EventType::LastWriteToPriorChannel,
                )]))
            }
            EventType::LastWriteToPriorChannel => {
                if self.last_received {
                    bail!("Duplicate last-write marker");
                }
                if self.next.is_none() {
                    let receive = self
                        .waiting
                        .as_mut()
                        .context("Last-write without an upgrade")?;
                    self.next = Some(
                        timeout(DEADLINE, receive)
                            .await
                            .context("Wi-Fi upgrade connection timed out")??,
                    );
                    self.waiting = None;
                }
                self.last_received = true;
                let mut frames = Vec::new();
                if !self.last_sent {
                    self.last_sent = true;
                    frames.push(event(EventType::LastWriteToPriorChannel));
                }
                frames.push(event(EventType::SafeToClosePriorChannel));
                Ok(Action::Send(frames))
            }
            EventType::SafeToClosePriorChannel
                if self.last_sent && self.last_received && self.next.is_some() =>
            {
                Ok(Action::Switch)
            }
            EventType::UpgradeFailure => {
                let path = packet
                    .upgrade_path_info
                    .as_ref()
                    .context("Missing failed upgrade path")?;
                let address = lan_address(
                    path.wifi_lan_socket
                        .as_ref()
                        .context("Missing failed Wi-Fi socket")?,
                )?;
                let context = self
                    .context
                    .as_ref()
                    .context("Missing failed upgrade context")?;
                if path.medium() != Medium::WifiLan
                    || self.waiting.is_none()
                    || self.last_sent
                    || self.last_received
                    || context.address != Some(address)
                {
                    bail!("Unsolicited or mismatched upgrade failure");
                }
                context
                    .pending
                    .lock()
                    .unwrap()
                    .remove(self.remote.as_deref().unwrap_or(""));
                self.waiting = None;
                self.deadline = None;
                self.lan_failed = true;
                warn!("Peer rejected LAN upgrade; retaining encrypted Bluetooth transport");
                Ok(Action::Send(Vec::new()))
            }
            _ => bail!("Out-of-order upgrade event {:?}", packet.event_type()),
        }
    }

    pub async fn switch(&mut self, old: &mut Transport) -> anyhow::Result<()> {
        // Google's optional closing frame is plaintext so it cannot advance crypto sequence numbers.
        let frame = wire::OfflineFrame {
            version: Some(1),
            v1: Some(wire::V1Frame {
                r#type: Some(wire::v1_frame::FrameType::Disconnection.into()),
                disconnection: Some(wire::DisconnectionFrame {
                    request_safe_to_disconnect: Some(false),
                    ack_safe_to_disconnect: Some(false),
                }),
                ..Default::default()
            }),
        };
        write_plain(old, &frame).await?;
        let _ = read_plain(old).await;
        *old = self.next.take().context("No pending Wi-Fi connection")?;
        self.complete = true;
        self.deadline = None;
        info!("Bluetooth -> Wi-Fi: handover complete, retaining UKEY2 keys and sequence numbers");
        Ok(())
    }
}

fn lan_address(socket: &WifiLanSocket) -> anyhow::Result<SocketAddrV4> {
    let bytes: [u8; 4] = socket
        .ip_address()
        .try_into()
        .context("Upgrade requires IPv4")?;
    let ip = Ipv4Addr::from(bytes);
    if !ip.is_private() || socket.wifi_port() <= 0 || socket.wifi_port() > 65535 {
        bail!("Invalid/nonlocal upgrade address");
    }
    Ok(SocketAddrV4::new(ip, socket.wifi_port() as u16))
}

pub(crate) fn event(kind: EventType) -> wire::OfflineFrame {
    wire::OfflineFrame {
        version: Some(1),
        v1: Some(wire::V1Frame {
            r#type: Some(wire::v1_frame::FrameType::BandwidthUpgradeNegotiation.into()),
            bandwidth_upgrade_negotiation: Some(wire::BandwidthUpgradeNegotiationFrame {
                event_type: Some(kind.into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
    }
}

async fn write_plain(stream: &mut Transport, frame: &wire::OfflineFrame) -> anyhow::Result<()> {
    timeout(DEADLINE, async {
        let bytes = frame.encode_to_vec();
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(&bytes).await?;
        stream.flush().await
    })
    .await??;
    Ok(())
}
async fn read_plain(stream: &mut Transport) -> anyhow::Result<wire::OfflineFrame> {
    timeout(DEADLINE, async {
        let length = stream.read_u32().await?;
        if length == 0 || length > 65536 {
            bail!("Invalid upgrade frame length");
        }
        let mut data = vec![0; length as usize];
        stream.read_exact(&mut data).await?;
        Ok(wire::OfflineFrame::decode(&*data)?)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_lan_keeps_old_channel_and_reports_the_rejected_path() {
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.deadline = Some(tokio::time::Instant::now());
        let path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiLan.into()),
            ..Default::default()
        };
        let Action::Send(frames) = upgrade.retain_bluetooth(&path) else {
            panic!("must notify peer");
        };
        let packet = frames[0]
            .v1
            .as_ref()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_ref()
            .unwrap();
        assert_eq!(packet.event_type(), EventType::UpgradeFailure);
        assert_eq!(packet.upgrade_path_info.as_ref(), Some(&path));
        assert!(upgrade.lan_failed);
        assert!(upgrade.deadline.is_none());
        assert!(!upgrade.last_sent && !upgrade.last_received && !upgrade.complete);
        assert!(upgrade.next.is_none());
    }

    #[test]
    fn peer_rejection_of_our_offer_preserves_bluetooth_and_cleans_pending() {
        let mut context = Context::new(*b"TEST", 9300);
        context.address = Some("10.42.77.1:9300".parse().unwrap());
        let mut upgrade = Upgrade::default();
        upgrade.context = Some(context.clone());
        upgrade.remote = Some("PEER".into());
        upgrade.bluetooth = true;
        let offer = upgrade.offer().unwrap().unwrap();
        let mut rejected = offer.v1.unwrap().bandwidth_upgrade_negotiation.unwrap();
        rejected.event_type = Some(EventType::UpgradeFailure.into());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(upgrade.handle(&rejected)).unwrap();
        assert!(upgrade.lan_failed);
        assert!(upgrade.deadline.is_none() && upgrade.waiting.is_none());
        assert!(context.pending.lock().unwrap().is_empty());
        assert!(!upgrade.complete && !upgrade.last_sent && !upgrade.last_received);
        assert!(runtime.block_on(upgrade.handle(&rejected)).is_err());
    }

    #[tokio::test]
    async fn offered_channel_drains_old_stream_before_switching() {
        let mut context = Context::new(*b"TEST", 9300);
        context.address = Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 9300));
        let mut upgrade = Upgrade::default();
        upgrade.context = Some(context.clone());
        upgrade.remote = Some("PEER".into());
        upgrade.bluetooth = true;
        let offer = upgrade.offer().unwrap().unwrap();
        let path = offer
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap()
            .upgrade_path_info
            .unwrap();
        assert_eq!(
            lan_address(path.wifi_lan_socket.as_ref().unwrap())
                .unwrap()
                .port(),
            9300
        );
        assert!(!path.supports_disabling_encryption());
        let (wifi, peer_wifi) = tokio::io::duplex(1024);
        let mut wifi = Transport::new(wifi);
        let mut peer_wifi = Transport::new(peer_wifi);
        let mut intro = event(EventType::ClientIntroduction);
        intro
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .client_introduction = Some(bwu::ClientIntroduction {
            endpoint_id: Some("PEER".into()),
            supports_disabling_encryption: Some(false),
        });
        assert!(context.route_introduction(&intro, &mut wifi).await.unwrap());
        assert_eq!(
            read_plain(&mut peer_wifi)
                .await
                .unwrap()
                .v1
                .unwrap()
                .bandwidth_upgrade_negotiation
                .unwrap()
                .event_type(),
            EventType::ClientIntroductionAck
        );
        let last = event(EventType::LastWriteToPriorChannel)
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap();
        let Action::Send(frames) = upgrade.handle(&last).await.unwrap() else {
            panic!("must drain old stream");
        };
        assert_eq!(frames.len(), 2);
        assert!(!upgrade.complete);
        let safe = event(EventType::SafeToClosePriorChannel)
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap();
        assert!(matches!(
            upgrade.handle(&safe).await.unwrap(),
            Action::Switch
        ));
        let (old, peer_old) = tokio::io::duplex(1024);
        let mut old = Transport::new(old);
        let mut peer_old = Transport::new(peer_old);
        let (local, remote) = tokio::join!(upgrade.switch(&mut old), async {
            let disconnect = read_plain(&mut peer_old).await.unwrap();
            assert_eq!(
                disconnect.v1.as_ref().unwrap().r#type(),
                wire::v1_frame::FrameType::Disconnection
            );
            write_plain(&mut peer_old, &disconnect).await
        });
        local.unwrap();
        remote.unwrap();
        assert!(upgrade.complete);
        old.write_all(b"file over Wi-Fi").await.unwrap();
        let mut data = [0; 15];
        peer_wifi.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"file over Wi-Fi");
        assert!(context.pending.lock().unwrap().is_empty());
    }
    #[test]
    fn upgrade_addresses_are_local_and_bounded() {
        for (ip, port, valid) in [
            (vec![10, 0, 0, 2], 9300, true),
            (vec![127, 0, 0, 1], 9300, false),
            (vec![8, 8, 8, 8], 9300, false),
            (vec![10; 16], 9300, false),
            (vec![10, 0, 0, 2], 65536, false),
            (vec![10, 0, 0, 2], 0, false),
        ] {
            assert_eq!(
                lan_address(&WifiLanSocket {
                    ip_address: Some(ip),
                    wifi_port: Some(port)
                })
                .is_ok(),
                valid
            );
        }
    }
    #[tokio::test]
    async fn refuses_unsolicited_and_out_of_order_upgrades() {
        let context = Context::new(*b"TEST", 9300);
        let (a, _b) = tokio::io::duplex(1024);
        let mut a = Transport::new(a);
        let mut intro = event(EventType::ClientIntroduction);
        intro
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .client_introduction = Some(bwu::ClientIntroduction {
            endpoint_id: Some("EVIL".into()),
            ..Default::default()
        });
        assert!(context.route_introduction(&intro, &mut a).await.is_err());
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        assert!(
            upgrade
                .handle(&wire::BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::SafeToClosePriorChannel.into()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn wifi_direct_and_hotspot_credentials_validation_and_handover() {
        use bwu::upgrade_path_info::{WifiDirectCredentials, WifiHotspotCredentials};

        // 1. Default 0.0.0.0 gateway resolves to 192.168.49.1 for WifiDirect and 192.168.43.1 for WifiHotspot
        let direct_path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiDirect.into()),
            wifi_direct_credentials: Some(WifiDirectCredentials {
                ssid: Some("DIRECT-xy-Android_1234".into()),
                password: Some("secretpass".into()),
                port: Some(9300),
                frequency: Some(5180),
                gateway: Some("0.0.0.0".into()),
            }),
            supports_disabling_encryption: Some(false),
            supports_client_introduction_ack: Some(true),
            ..Default::default()
        };
        let req = parse_p2p_upgrade_request(&direct_path).unwrap();
        assert_eq!(req.medium, Medium::WifiDirect);
        assert_eq!(req.ssid, "DIRECT-xy-Android_1234");
        assert_eq!(req.gateway, Ipv4Addr::new(192, 168, 49, 1));
        assert_eq!(req.port, 9300);
        assert_eq!(req.frequency, Some(5180));

        let hotspot_path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiHotspot.into()),
            wifi_hotspot_credentials: Some(WifiHotspotCredentials {
                ssid: Some("AndroidShare_5678".into()),
                password: Some("hotspotpass".into()),
                port: Some(9301),
                gateway: None,
                frequency: Some(2412),
            }),
            supports_disabling_encryption: Some(false),
            ..Default::default()
        };
        let h_req = parse_p2p_upgrade_request(&hotspot_path).unwrap();
        assert_eq!(h_req.medium, Medium::WifiHotspot);
        assert_eq!(h_req.gateway, Ipv4Addr::new(192, 168, 43, 1));
        assert_eq!(h_req.port, 9301);

        // 2. Rejects public gateway or empty credentials
        let bad_path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiDirect.into()),
            wifi_direct_credentials: Some(WifiDirectCredentials {
                ssid: Some("DIRECT-xy".into()),
                password: Some("secret".into()),
                port: Some(9300),
                frequency: None,
                gateway: Some("8.8.8.8".into()),
            }),
            ..Default::default()
        };
        assert!(parse_p2p_upgrade_request(&bad_path).is_err());

        // 3. End-to-end P2P upgrader invocation and handover over local private IP listener
        let recorded = Arc::new(Mutex::new(None));
        let recorded_clone = recorded.clone();
        set_p2p_upgrader(Arc::new(move |r| {
            let recorded_clone = recorded_clone.clone();
            Box::pin(async move {
                *recorded_clone.lock().unwrap() = Some(r);
                Ok(())
            })
        }));

        if let Some(local_private_ip) = get_if_addrs::get_if_addrs().ok().and_then(|ifs| {
            ifs.into_iter().find_map(|i| match i.ip() {
                std::net::IpAddr::V4(ip) if ip.is_private() => Some(ip),
                _ => None,
            })
        }) {
            let listener =
                tokio::net::TcpListener::bind(SocketAddrV4::new(local_private_ip, 0))
                    .await
                    .unwrap();
            let port = listener.local_addr().unwrap().port();
            let peer_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = Transport::new(stream);
                let intro = read_plain(&mut stream).await.unwrap();
                let intro_frame = intro
                    .v1
                    .unwrap()
                    .bandwidth_upgrade_negotiation
                    .unwrap()
                    .client_introduction
                    .unwrap();
                assert_eq!(intro_frame.endpoint_id(), "TEST");
                let ack = event(EventType::ClientIntroductionAck);
                write_plain(&mut stream, &ack).await.unwrap();
            });

            let mut upgrade = Upgrade::default();
            upgrade.context = Some(Context::new(*b"TEST", 9300));
            upgrade.bluetooth = true;
            let frame = wire::BandwidthUpgradeNegotiationFrame {
                event_type: Some(EventType::UpgradePathAvailable.into()),
                upgrade_path_info: Some(bwu::UpgradePathInfo {
                    medium: Some(Medium::WifiDirect.into()),
                    wifi_direct_credentials: Some(WifiDirectCredentials {
                        ssid: Some("DIRECT-xy-Phone".into()),
                        password: Some("pass1234".into()),
                        port: Some(i32::from(port)),
                        frequency: Some(5180),
                        gateway: Some(local_private_ip.to_string()),
                    }),
                    supports_disabling_encryption: Some(false),
                    supports_client_introduction_ack: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let action = upgrade.handle(&frame).await.unwrap();
            assert!(matches!(action, Action::Send(ref f) if f.len() == 1));
            assert!(upgrade.next.is_some() && upgrade.last_sent);
            peer_task.await.unwrap();
            let captured = recorded.lock().unwrap().clone().unwrap();
            assert_eq!(captured.ssid, "DIRECT-xy-Phone");
            assert_eq!(captured.port, port);
        }
    }
}
