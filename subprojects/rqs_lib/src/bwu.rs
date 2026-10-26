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
// Fast probe window for Wi-Fi LAN upgrade before falling back to Wi-Fi Hotspot / Direct.
const LAN_UPGRADE_DEADLINE: Duration = Duration::from_millis(3500);
// Android may enable Wi-Fi and retry hotspot association for over 30 seconds.
// Android aborted at 60s from handshake; leave time for fallback after its ~30s join attempt.
const DIRECT_JOIN_DEADLINE: Duration = Duration::from_secs(40);
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Transport>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pUpgradeRequest {
    pub medium: Medium,
    pub ssid: String,
    pub device_name: Option<String>,
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

#[allow(dead_code)]
pub fn clear_p2p_upgrader() {
    if let Ok(mut guard) = P2P_UPGRADER.write() {
        *guard = None;
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
            let device_name = creds.device_name.clone().filter(|name| !name.is_empty());
            if let Some(name) = &device_name {
                if name.is_empty() || name.len() > 32 || name.chars().any(char::is_control) {
                    bail!("Invalid Wi-Fi Direct device name");
                }
            } else if ssid.is_empty() || ssid.len() > 32 || password.is_empty() {
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
                gw_str
                    .parse::<Ipv4Addr>()
                    .context("Invalid Wi-Fi Direct gateway")?
            };
            if !gateway.is_private() {
                bail!("Non-private Wi-Fi Direct gateway");
            }
            Ok(P2pUpgradeRequest {
                medium: Medium::WifiDirect,
                device_name,
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
                gw_str
                    .parse::<Ipv4Addr>()
                    .context("Invalid Wi-Fi Hotspot gateway")?
            };
            if !gateway.is_private() {
                bail!("Non-private Wi-Fi Hotspot gateway");
            }
            Ok(P2pUpgradeRequest {
                medium: Medium::WifiHotspot,
                device_name: None,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiDirectOffer {
    pub medium: Medium,
    pub ssid: String,
    pub password: String,
    pub gateway: Ipv4Addr,
    pub port: u16,
    pub frequency: i32,
}

pub type P2pHostProvider = Arc<
    dyn Fn(
            u16,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<WifiDirectOffer>> + Send>>
        + Send
        + Sync,
>;

static P2P_HOST_PROVIDER: std::sync::RwLock<Option<P2pHostProvider>> = std::sync::RwLock::new(None);

pub fn set_p2p_host_provider(provider: P2pHostProvider) {
    if let Ok(mut guard) = P2P_HOST_PROVIDER.write() {
        *guard = Some(provider);
    }
}

pub(crate) fn spawn_host_preparation(port: u16) {
    let provider = P2P_HOST_PROVIDER
        .read()
        .ok()
        .and_then(|guard| guard.clone());
    if let Some(provider) = provider {
        tokio::spawn(async move {
            if provider(port).await.is_some() {
                info!("Direct Wi-Fi host prepared before transfer consent");
            }
        });
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub endpoint: String,
    pub address: Option<SocketAddrV4>,
    pub wifi_direct: Option<WifiDirectOffer>,
    pub port: u16,
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
            .filter(|_| !bluetooth_only && std::env::var("RQS_SKIP_LAN").as_deref() != Ok("1"))
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
            wifi_direct: None,
            port,
            pending: Arc::default(),
        }
    }

    pub async fn resolve_wifi_direct(&self) -> Option<WifiDirectOffer> {
        if let Some(offer) = self.wifi_direct.clone() {
            return Some(offer);
        }
        let provider = P2P_HOST_PROVIDER.read().ok()?.clone()?;
        provider(self.port).await
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
            debug!("Peer indicated supports_disabling_encryption; retaining UKEY2 encryption");
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
            if let Some(sender) = pending.remove(intro.endpoint_id()) {
                Some(sender)
            } else if pending.len() == 1 {
                let key = pending.keys().next().cloned();
                key.and_then(|k| pending.remove(&k))
            } else {
                None
            }
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
    pub client_only: bool,
    pub active_medium: Option<Medium>,
    pub lan_failed: bool,
    pub lan_attempted: bool,
    pub direct_failed: bool,
    pub direct_offered: Option<WifiDirectOffer>,
    pub next: Option<Transport>,
    pub waiting: Option<oneshot::Receiver<Transport>>,
    pub last_sent: bool,
    pub deferred: Vec<wire::OfflineFrame>,
    last_received: bool,
    pub complete: bool,
    pub link_encryption: bool,
    pub deadline: Option<tokio::time::Instant>,
}

impl Drop for Upgrade {
    fn drop(&mut self) {
        if self.waiting.is_some() {
            if let Some(context) = &self.context {
                let mut pending = context.pending.lock().unwrap();
                let remote = self.remote.as_deref().unwrap_or("");
                if pending.remove(remote).is_none() && pending.len() == 1 {
                    let key = pending.keys().next().cloned();
                    if let Some(k) = key {
                        pending.remove(&k);
                    }
                }
            }
        }
    }
}

pub(crate) enum Action {
    Send(Vec<wire::OfflineFrame>),
    Switch,
}

impl Upgrade {
    pub fn uses_link_encryption(&self) -> bool {
        self.complete && self.link_encryption && self.active_medium == Some(Medium::WifiDirect)
    }

    pub fn medium_label(&self) -> &'static str {
        if self.complete {
            match self.active_medium {
                Some(Medium::WifiDirect) => "Wi-Fi Direct",
                Some(Medium::WifiHotspot) => "Wi-Fi Hotspot",
                _ => "Wi-Fi LAN",
            }
        } else if self.waiting.is_some() || self.last_sent || self.next.is_some() {
            match self.active_medium {
                Some(Medium::WifiDirect) => "Upgrading to Wi-Fi Direct…",
                Some(Medium::WifiHotspot) => "Upgrading to Hotspot…",
                _ => "Upgrading to Wi-Fi…",
            }
        } else if self.bluetooth {
            "Bluetooth"
        } else {
            "Wi-Fi LAN"
        }
    }

    fn retain_bluetooth(&mut self, path: &bwu::UpgradePathInfo) -> Action {
        self.lan_failed = true;
        self.active_medium = Some(Medium::Bluetooth);
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

    pub async fn offer(&mut self) -> anyhow::Result<Option<wire::OfflineFrame>> {
        if !self.bluetooth || self.complete || self.waiting.is_some() || self.next.is_some() {
            return Ok(None);
        }
        let Some(context) = self.context.as_ref() else {
            return Ok(None);
        };

        // 1. Primary receiver offer: local Wi-Fi LAN if available and not yet failed/attempted.
        if !self.lan_attempted && !self.lan_failed && self.direct_offered.is_none() {
            if let Some(address) = context.address {
                let remote = self.remote.as_deref().unwrap_or("").to_owned();
                let (tx, rx) = oneshot::channel();
                let mut pending = context.pending.lock().unwrap();
                if !remote.is_empty() && pending.contains_key(&remote) {
                    bail!("Endpoint already upgrading");
                }
                pending.insert(remote, tx);
                drop(pending);
                self.waiting = Some(rx);
                self.active_medium = Some(Medium::WifiLan);
                self.deadline = Some(tokio::time::Instant::now() + LAN_UPGRADE_DEADLINE);
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
                return Ok(Some(frame));
            }
        }

        // 2. Receiver-initiated Wi-Fi Direct GO offer when LAN is unavailable or rejected (e.g. AP isolation).
        if !self.direct_failed && !self.client_only {
            if let Some(direct) = &self.direct_offered {
                if self.waiting.is_none() {
                    let remote = self.remote.as_deref().unwrap_or("").to_owned();
                    let (tx, rx) = oneshot::channel();
                    let mut pending = context.pending.lock().unwrap();
                    if !remote.is_empty() {
                        pending.remove(&remote);
                    }
                    pending.insert(remote, tx);
                    drop(pending);
                    self.waiting = Some(rx);
                    self.active_medium = Some(direct.medium);
                    self.deadline = Some(tokio::time::Instant::now() + DIRECT_JOIN_DEADLINE);
                }
                info!(
                    "Bluetooth -> {:?}: re-offering local host {} ({}:{}, {} MHz)",
                    direct.medium, direct.ssid, direct.gateway, direct.port, direct.frequency
                );
                return Ok(Some(Self::build_direct_offer_frame(direct)));
            } else if let Some(direct) = context.resolve_wifi_direct().await {
                if !matches!(direct.medium, Medium::WifiDirect | Medium::WifiHotspot)
                    || !direct.gateway.is_private()
                    || direct.ssid.is_empty()
                    || direct.ssid.len() > 32
                    || direct.password.is_empty()
                    || direct.port == 0
                {
                    bail!("Invalid local Wi-Fi Direct offer configuration");
                }
                let remote = self.remote.as_deref().unwrap_or("").to_owned();
                let (tx, rx) = oneshot::channel();
                let mut pending = context.pending.lock().unwrap();
                if !remote.is_empty() {
                    pending.remove(&remote);
                }
                pending.insert(remote, tx);
                drop(pending);
                self.waiting = Some(rx);
                self.active_medium = Some(direct.medium);
                self.deadline = Some(tokio::time::Instant::now() + DIRECT_JOIN_DEADLINE);
                self.direct_offered = Some(direct.clone());
                info!(
                    "Bluetooth -> {:?}: offering local host {} ({}:{}, {} MHz)",
                    direct.medium, direct.ssid, direct.gateway, direct.port, direct.frequency
                );
                return Ok(Some(Self::build_direct_offer_frame(&direct)));
            }
        }

        Ok(None)
    }

    pub fn build_direct_offer_frame(direct: &WifiDirectOffer) -> wire::OfflineFrame {
        let mut frame = event(EventType::UpgradePathAvailable);
        frame
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .upgrade_path_info = Some(bwu::UpgradePathInfo {
            medium: Some(direct.medium.into()),
            wifi_hotspot_credentials: (direct.medium == Medium::WifiHotspot).then(|| {
                bwu::upgrade_path_info::WifiHotspotCredentials {
                    ssid: Some(direct.ssid.clone()),
                    password: Some(direct.password.clone()),
                    port: Some(i32::from(direct.port)),
                    frequency: Some(direct.frequency),
                    gateway: Some(direct.gateway.to_string()),
                }
            }),
            wifi_direct_credentials: (direct.medium == Medium::WifiDirect).then(|| {
                bwu::upgrade_path_info::WifiDirectCredentials {
                    ssid: Some(direct.ssid.clone()),
                    password: Some(direct.password.clone()),
                    port: Some(i32::from(direct.port)),
                    frequency: Some(direct.frequency),
                    gateway: Some(direct.gateway.to_string()),
                    ..Default::default()
                }
            }),
            supports_disabling_encryption: Some(false),
            supports_client_introduction_ack: Some(true),
            ..Default::default()
        });
        frame
    }

    pub async fn on_deadline_expired(&mut self) -> anyhow::Result<Option<wire::OfflineFrame>> {
        if self.client_only && self.waiting.is_none() && self.next.is_none()
            && !self.last_sent && !self.last_received
        {
            self.deadline = None;
            self.lan_failed = true;
            self.active_medium = Some(Medium::Bluetooth);
            return Ok(None);
        }
        if self.active_medium == Some(Medium::WifiLan)
            && self.direct_offered.is_none()
            && !self.last_sent
            && !self.last_received
        {
            if let Some(context) = &self.context {
                context
                    .pending
                    .lock()
                    .unwrap()
                    .remove(self.remote.as_deref().unwrap_or(""));
            }
            self.waiting = None;
            self.deadline = None;
            self.lan_attempted = true;
            if let Some(direct_offer_frame) = self.offer().await? {
                info!(
                    "Wi-Fi LAN upgrade probe timed out; automatically falling back to receiver-initiated Wi-Fi Hotspot/Direct offer"
                );
                return Ok(Some(direct_offer_frame));
            }
            self.lan_failed = true;
            self.active_medium = Some(Medium::Bluetooth);
            warn!(
                "Wi-Fi LAN upgrade probe timed out and no Wi-Fi Direct host is available; retaining encrypted Bluetooth transport"
            );
            return Ok(None);
        }
        if matches!(
            self.active_medium,
            Some(Medium::WifiHotspot | Medium::WifiDirect)
        ) && self.waiting.is_some()
            && !self.last_sent
            && !self.last_received
        {
            if let Some(context) = &self.context {
                context
                    .pending
                    .lock()
                    .unwrap()
                    .remove(self.remote.as_deref().unwrap_or(""));
            }
            self.waiting = None;
            self.deadline = None;
            self.direct_offered = None;
            self.direct_failed = true;
            self.lan_failed = true;
            self.active_medium = Some(Medium::Bluetooth);
            warn!(
                "Wi-Fi Hotspot/Direct offer timed out without peer join; retaining encrypted Bluetooth transport"
            );
            return Ok(None);
        }
        bail!("Wi-Fi handover timed out")
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
                if self.next.is_some() {
                    bail!("Channel upgrade already active");
                }
                if self.waiting.is_some() {
                    if self.active_medium == Some(Medium::WifiLan) {
                        warn!(
                            "Simultaneous upgrade (glare): received peer UpgradePathAvailable while awaiting LAN offer; cancelling LAN offer"
                        );
                        if let Some(context) = &self.context {
                            let mut pending = context.pending.lock().unwrap();
                            let remote = self.remote.as_deref().unwrap_or("");
                            pending.remove(remote);
                        }
                        self.waiting = None;
                        self.lan_failed = true;
                    } else {
                        debug!("Received peer UpgradePathAvailable while already awaiting Wi-Fi Hotspot join; retaining Hotspot listener");
                        let path = packet.upgrade_path_info.as_ref()
                            .context("Missing upgrade path")?;
                        let mut failure = event(EventType::UpgradeFailure);
                        failure.v1.as_mut().unwrap()
                            .bandwidth_upgrade_negotiation.as_mut().unwrap()
                            .upgrade_path_info = Some(path.clone());
                        // Joining another group would tear down the host the peer may still be joining.
                        return Ok(Action::Send(vec![failure]));
                    }
                }
                let path = packet
                    .upgrade_path_info
                    .as_ref()
                    .context("Missing upgrade path")?;
                let mut link_encryption = false;
                let address = match path.medium() {
                    Medium::WifiLan => lan_address(
                        path.wifi_lan_socket
                            .as_ref()
                            .context("Missing Wi-Fi LAN socket")?,
                    )?,
                    Medium::WifiDirect | Medium::WifiHotspot => {
                        if self.direct_failed {
                            debug!("P2P already failed in this transfer; refusing repeated privileged setup");
                            return Ok(self.retain_bluetooth(path));
                        }
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
                        link_encryption = req.medium == Medium::WifiDirect
                            && req.device_name.is_none() && path.supports_disabling_encryption();
                        info!(
                            "Received {:?} upgrade offer: ssid={}, device_name={:?}, gateway={}, port={}, freq={:?}",
                            req.medium, req.ssid, req.device_name, req.gateway, req.port, req.frequency
                        );
                        let upgrader = P2P_UPGRADER.read().ok().and_then(|g| g.clone());
                        let p2p_prepared = if let Some(upgrader) = upgrader {
                            match upgrader(req.clone()).await {
                                Ok(()) => true,
                                Err(error) => {
                                    warn!("Wi-Fi P2P link preparation returned warning: {error:#}");
                                    false
                                }
                            }
                        } else {
                            false
                        };
                        if !p2p_prepared {
                            if self.direct_offered.is_none() && !self.direct_failed {
                                warn!(
                                    "Cannot join peer {:?} network (no active P2P client link); offering local Wi-Fi Hotspot instead",
                                    req.medium
                                );
                                self.lan_failed = true;
                                self.lan_attempted = true;
                                let mut failure = event(EventType::UpgradeFailure);
                                failure
                                    .v1
                                    .as_mut()
                                    .unwrap()
                                    .bandwidth_upgrade_negotiation
                                    .as_mut()
                                    .unwrap()
                                    .upgrade_path_info = Some(path.clone());
                                if let Ok(Some(offer)) = self.offer().await {
                                    return Ok(Action::Send(vec![failure, offer]));
                                }
                            }
                            if let Some(context) = &self.context {
                                context
                                    .pending
                                    .lock()
                                    .unwrap()
                                    .remove(self.remote.as_deref().unwrap_or(""));
                            }
                            self.waiting = None;
                            self.direct_offered = None;
                            self.direct_failed = true;
                            warn!(
                                "Peer did not join local Wi-Fi Hotspot and cannot join peer {:?} network; retaining encrypted Bluetooth transport",
                                req.medium
                            );
                            return Ok(self.retain_bluetooth(path));
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
                self.deadline = Some(tokio::time::Instant::now() + DEADLINE);
                let connection = Connector::from_env()?.connect(&address.to_string()).await;
                let mut socket = match connection {
                    Ok(socket) => Transport::new(socket),
                    Err(error) => {
                        warn!(
                            "Wi-Fi handover to {address} failed; checking Wi-Fi Direct fallback: {error:#}"
                        );
                        self.lan_failed = true;
                        self.lan_attempted = true;
                        if let Ok(Some(offer)) = self.offer().await {
                            info!(
                                "Peer LAN unreachable (e.g. AP isolation); sender proposing Wi-Fi Direct offer"
                            );
                            return Ok(Action::Send(vec![offer]));
                        }
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
                    supports_disabling_encryption: Some(link_encryption),
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
                self.active_medium = Some(path.medium());
                self.link_encryption = link_encryption;
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
                if self.waiting.is_none() || self.last_sent || self.last_received {
                    bail!("Unsolicited or mismatched upgrade failure");
                }
                match path.medium() {
                    Medium::WifiLan => {
                        let address = lan_address(
                            path.wifi_lan_socket
                                .as_ref()
                                .context("Missing failed Wi-Fi socket")?,
                        )?;
                        let context = self
                            .context
                            .as_ref()
                            .context("Missing failed upgrade context")?;
                        if self.direct_offered.is_some() || context.address != Some(address) {
                            bail!("Unsolicited or mismatched upgrade failure");
                        }
                        context
                            .pending
                            .lock()
                            .unwrap()
                            .remove(self.remote.as_deref().unwrap_or(""));
                        self.waiting = None;
                        self.deadline = None;
                        self.lan_attempted = true;
                        if let Some(direct_offer_frame) = self.offer().await? {
                            info!(
                                "Peer rejected LAN upgrade (e.g. AP isolation); falling back to receiver-initiated Wi-Fi Direct offer"
                            );
                            return Ok(Action::Send(vec![direct_offer_frame]));
                        }
                        self.lan_failed = true;
                        self.active_medium = Some(Medium::Bluetooth);
                        warn!("Peer rejected LAN upgrade; retaining encrypted Bluetooth transport");
                        Ok(Action::Send(Vec::new()))
                    }
                    Medium::WifiDirect | Medium::WifiHotspot => {
                        let req = parse_p2p_upgrade_request(path)?;
                        let offered = self
                            .direct_offered
                            .as_ref()
                            .context("Unsolicited Wi-Fi Direct upgrade failure")?;
                        if offered.medium != req.medium
                            || offered.ssid != req.ssid
                            || offered.gateway != req.gateway
                            || offered.port != req.port
                        {
                            bail!("Mismatched Wi-Fi Direct upgrade failure");
                        }
                        let context = self
                            .context
                            .as_ref()
                            .context("Missing failed upgrade context")?;
                        context
                            .pending
                            .lock()
                            .unwrap()
                            .remove(self.remote.as_deref().unwrap_or(""));
                        self.waiting = None;
                        self.deadline = None;
                        self.direct_offered = None;
                        self.direct_failed = true;
                        self.lan_failed = true;
                        self.active_medium = Some(Medium::Bluetooth);
                        warn!(
                            "Peer rejected Wi-Fi Direct upgrade; retaining encrypted Bluetooth transport"
                        );
                        Ok(Action::Send(Vec::new()))
                    }
                    _ => bail!("Unsolicited or mismatched upgrade failure"),
                }
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
        let _ = timeout(Duration::from_millis(200), async {
            if write_plain(old, &frame).await.is_ok() {
                let _ = read_plain(old).await;
            }
        })
        .await;
        *old = self.next.take().context("No pending Wi-Fi connection")?;
        self.complete = true;
        self.deadline = None;
        info!("Bluetooth -> Wi-Fi: handover complete; link_encryption={}, UKEY2 context retained",
            self.uses_link_encryption());
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

    #[tokio::test]
    async fn peer_rejection_of_our_offer_preserves_bluetooth_and_cleans_pending() {
        let mut context = Context::new(*b"TEST", 9300);
        context.address = Some("10.42.77.1:9300".parse().unwrap());
        let mut upgrade = Upgrade::default();
        upgrade.context = Some(context.clone());
        upgrade.remote = Some("PEER".into());
        upgrade.bluetooth = true;
        let offer = upgrade.offer().await.unwrap().unwrap();
        let mut rejected = offer.v1.unwrap().bandwidth_upgrade_negotiation.unwrap();
        rejected.event_type = Some(EventType::UpgradeFailure.into());
        upgrade.handle(&rejected).await.unwrap();
        assert!(upgrade.lan_failed);
        assert!(upgrade.deadline.is_none() && upgrade.waiting.is_none());
        assert!(context.pending.lock().unwrap().is_empty());
        assert!(!upgrade.complete && !upgrade.last_sent && !upgrade.last_received);
        assert!(upgrade.handle(&rejected).await.is_err());
    }

    #[tokio::test]
    async fn offered_channel_drains_old_stream_before_switching() {
        let mut context = Context::new(*b"TEST", 9300);
        context.address = Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 9300));
        let mut upgrade = Upgrade::default();
        upgrade.context = Some(context.clone());
        upgrade.remote = Some("PEER".into());
        upgrade.bluetooth = true;
        let offer = upgrade.offer().await.unwrap().unwrap();
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
    async fn failed_direct_client_does_not_repeat_privileged_authorization() {
        let _guard = TEST_P2P_MUTEX.lock().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorded = calls.clone();
        set_p2p_upgrader(Arc::new(move |_| {
            recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("previously failed") })
        }));
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.direct_failed = true;
        let frame = wire::BandwidthUpgradeNegotiationFrame {
            event_type: Some(EventType::UpgradePathAvailable.into()),
            upgrade_path_info: Some(bwu::UpgradePathInfo {
                medium: Some(Medium::WifiDirect.into()),
                wifi_direct_credentials: Some(bwu::upgrade_path_info::WifiDirectCredentials {
                    ssid: Some("DIRECT-test".into()), password: Some("testpass123".into()),
                    port: Some(9300), ..Default::default()
                }), ..Default::default()
            }), ..Default::default()
        };
        for _ in 0..2 { upgrade.handle(&frame).await.unwrap(); }
        clear_p2p_upgrader();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn empty_device_name_preserves_valid_legacy_credentials() {
        let path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiDirect.into()),
            wifi_direct_credentials: Some(bwu::upgrade_path_info::WifiDirectCredentials {
                ssid: Some("DIRECT-test".into()), password: Some("testpass123".into()),
                device_name: Some(String::new()), port: Some(9300), ..Default::default()
            }), ..Default::default()
        };
        let req = parse_p2p_upgrade_request(&path).unwrap();
        assert_eq!(req.device_name, None);
        assert_eq!(req.ssid, "DIRECT-test");
    }

    #[test]
    fn device_name_credentials_use_the_windows_wire_field_without_a_password() {
        use prost::Message;
        // Raw field 9, length 5, "Phone"; protects interoperability independently of our encoder.
        let creds = bwu::upgrade_path_info::WifiDirectCredentials::decode(
            &[0x4a, 5, b'P', b'h', b'o', b'n', b'e'][..]).unwrap();
        let mut path = bwu::UpgradePathInfo {
            medium: Some(Medium::WifiDirect.into()),
            wifi_direct_credentials: Some(bwu::upgrade_path_info::WifiDirectCredentials {
                port: Some(9300), ..creds
            }),
            ..Default::default()
        };
        let request = parse_p2p_upgrade_request(&path).unwrap();
        assert_eq!(request.device_name.as_deref(), Some("Phone"));
        assert!(request.ssid.is_empty() && request.password.is_empty());
        for name in ["", "Bad\nName", &"x".repeat(33)] {
            path.wifi_direct_credentials.as_mut().unwrap().device_name = Some(name.into());
            assert!(parse_p2p_upgrade_request(&path).is_err());
        }
        let creds = path.wifi_direct_credentials.as_mut().unwrap();
        creds.device_name = Some("Phone".into());
        creds.port = Some(65536);
        assert!(parse_p2p_upgrade_request(&path).is_err());
    }

    static TEST_P2P_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn wifi_direct_and_hotspot_credentials_validation_and_handover() {
        let _guard = TEST_P2P_MUTEX.lock().await;
        clear_p2p_upgrader();
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
                    ..Default::default()
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
                    ..Default::default()
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
            let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(local_private_ip, 0))
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
                assert!(intro_frame.supports_disabling_encryption());
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
                    ..Default::default()
                    }),
                    supports_disabling_encryption: Some(true),
                    supports_client_introduction_ack: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let action = upgrade.handle(&frame).await.unwrap();
            assert!(matches!(action, Action::Send(ref f) if f.len() == 1));
            assert!(upgrade.next.is_some() && upgrade.last_sent);
            assert!(upgrade.link_encryption);
            assert!(!upgrade.uses_link_encryption(), "Old channel must retain UKEY2 until switch");
            upgrade.complete = true;
            assert!(upgrade.uses_link_encryption());
            upgrade.active_medium = Some(Medium::WifiLan);
            assert!(!upgrade.uses_link_encryption(), "LAN never inherits direct-link mode");
            peer_task.await.unwrap();
            let captured = recorded.lock().unwrap().clone().unwrap();
            assert_eq!(captured.ssid, "DIRECT-xy-Phone");
            assert_eq!(captured.port, port);
        }
        clear_p2p_upgrader();
    }

    #[tokio::test]
    async fn receiver_initiated_wifi_direct_offer_and_ap_isolation_fallback() {
        // 1. AP-isolation scenario: LAN is offered first, rejected by peer, and automatically falls back to offering Wi-Fi Direct GO
        let mut context = Context::new(*b"RCVR", 9300);
        context.address = Some("10.0.0.10:9300".parse().unwrap());
        context.wifi_direct = Some(WifiDirectOffer {
            medium: Medium::WifiDirect,
            ssid: "DIRECT-PL-PacketLab".into(),
            password: "p2ppassword".into(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 5180,
        });

        let mut upgrade = Upgrade::default();
        upgrade.context = Some(context.clone());
        upgrade.remote = Some("PHON".into());
        upgrade.bluetooth = true;

        // Initial offer is WifiLan
        let lan_offer = upgrade.offer().await.unwrap().unwrap();
        let lan_info = lan_offer
            .v1
            .as_ref()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_ref()
            .unwrap()
            .upgrade_path_info
            .as_ref()
            .unwrap();
        assert_eq!(lan_info.medium(), Medium::WifiLan);

        // Peer rejects WifiLan due to AP isolation -> Upgrade::handle automatically sends WifiDirect offer
        let mut lan_rejected = lan_offer.v1.unwrap().bandwidth_upgrade_negotiation.unwrap();
        lan_rejected.event_type = Some(EventType::UpgradeFailure.into());
        let Action::Send(fallback_frames) = upgrade.handle(&lan_rejected).await.unwrap() else {
            panic!("expected automatic Wi-Fi Direct fallback offer");
        };
        assert_eq!(fallback_frames.len(), 1);
        assert!(!upgrade.lan_failed && upgrade.waiting.is_some());
        let direct_neg = fallback_frames[0]
            .v1
            .as_ref()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_ref()
            .unwrap();
        assert_eq!(direct_neg.event_type(), EventType::UpgradePathAvailable);
        let direct_path = direct_neg.upgrade_path_info.as_ref().unwrap();
        assert_eq!(direct_path.medium(), Medium::WifiDirect);
        let direct_creds = direct_path.wifi_direct_credentials.as_ref().unwrap();
        assert_eq!(direct_creds.ssid.as_deref(), Some("DIRECT-PL-PacketLab"));
        assert_eq!(direct_creds.password.as_deref(), Some("p2ppassword"));
        assert_eq!(direct_creds.gateway.as_deref(), Some("192.168.49.1"));
        assert_eq!(direct_creds.port, Some(9300));
        assert_eq!(direct_creds.frequency, Some(5180));

        // Peer connects to Receiver's Wi-Fi Direct GO and completes ClientIntroduction + channel switch
        let (p2p_local, p2p_peer) = tokio::io::duplex(1024);
        let mut p2p_local = Transport::new(p2p_local);
        let mut p2p_peer = Transport::new(p2p_peer);
        let mut intro = event(EventType::ClientIntroduction);
        intro
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .client_introduction = Some(bwu::ClientIntroduction {
            endpoint_id: Some("PHON".into()),
            supports_disabling_encryption: Some(false),
        });
        assert!(
            context
                .route_introduction(&intro, &mut p2p_local)
                .await
                .unwrap()
        );
        let ack = read_plain(&mut p2p_peer).await.unwrap();
        assert_eq!(
            ack.v1
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
        let Action::Send(drain_frames) = upgrade.handle(&last).await.unwrap() else {
            panic!("must drain old stream");
        };
        assert_eq!(drain_frames.len(), 2);
        let safe = event(EventType::SafeToClosePriorChannel)
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap();
        assert!(matches!(
            upgrade.handle(&safe).await.unwrap(),
            Action::Switch
        ));

        // 2. Direct-only scenario (no Wi-Fi LAN connected) where peer also rejects Wi-Fi Direct -> cleanly retains Bluetooth
        let mut context_no_lan = Context::new(*b"RCV2", 9300);
        context_no_lan.address = None;
        context_no_lan.wifi_direct = context.wifi_direct.clone();
        let mut upgrade2 = Upgrade::default();
        upgrade2.context = Some(context_no_lan.clone());
        upgrade2.remote = Some("PHN2".into());
        upgrade2.bluetooth = true;

        let direct_offer = upgrade2.offer().await.unwrap().unwrap();
        let mut direct_rejected = direct_offer
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap();
        direct_rejected.event_type = Some(EventType::UpgradeFailure.into());
        let Action::Send(empty) = upgrade2.handle(&direct_rejected).await.unwrap() else {
            panic!("expected empty send on final fallback");
        };
        assert!(empty.is_empty());
        assert!(upgrade2.direct_failed && upgrade2.lan_failed);
        assert!(upgrade2.waiting.is_none() && upgrade2.deadline.is_none());
        assert!(context_no_lan.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hotspot_offer_uses_hotspot_credentials_and_preserves_bluetooth_on_rejection() {
        let mut context = Context::new(*b"HOST", 9300);
        context.address = None;
        context.wifi_direct = Some(WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: "PacketLab-test".into(),
            password: "testpassword".into(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 2462,
        });
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.remote = Some("PEER".into());
        upgrade.context = Some(context.clone());
        let frame = upgrade.offer().await.unwrap().unwrap();
        let mut negotiation = frame.v1.unwrap().bandwidth_upgrade_negotiation.unwrap();
        let path = negotiation.upgrade_path_info.as_ref().unwrap();
        assert_eq!(path.medium(), Medium::WifiHotspot);
        assert!(
            upgrade.deadline.unwrap() > tokio::time::Instant::now() + Duration::from_secs(30),
            "Keep the host alive through Android's 30-second hotspot join attempts"
        );
        assert!(
            upgrade.deadline.unwrap() < tokio::time::Instant::now() + Duration::from_secs(45),
            "Leave time for payload fallback before Android's observed 60-second receive timeout"
        );
        assert!(path.wifi_direct_credentials.is_none());
        let parsed = parse_p2p_upgrade_request(path).unwrap();
        assert_eq!(parsed.gateway, Ipv4Addr::new(192, 168, 49, 1));
        assert_eq!(parsed.frequency, Some(2462));
        assert_eq!(parsed.ssid, "PacketLab-test");
        negotiation.event_type = Some(EventType::UpgradeFailure.into());
        assert!(
            matches!(upgrade.handle(&negotiation).await.unwrap(), Action::Send(f) if f.is_empty())
        );
        assert!(upgrade.lan_failed && upgrade.direct_failed);
        assert!(context.pending.lock().unwrap().is_empty());
        assert_eq!(upgrade.medium_label(), "Bluetooth");
    }

    #[test]
    fn transport_medium_label_tracks_upgrade_lifecycle() {
        let mut upgrade = Upgrade::default();
        assert_eq!(upgrade.medium_label(), "Wi-Fi LAN");
        upgrade.bluetooth = true;
        assert_eq!(upgrade.medium_label(), "Bluetooth");
        upgrade.active_medium = Some(Medium::WifiDirect);
        upgrade.last_sent = true;
        assert_eq!(upgrade.medium_label(), "Upgrading to Wi-Fi Direct…");
        upgrade.complete = true;
        assert_eq!(upgrade.medium_label(), "Wi-Fi Direct");
    }

    #[tokio::test]
    async fn lan_probe_timeout_automatically_falls_back_to_wifi_hotspot() {
        let _guard = TEST_P2P_MUTEX.lock().await;
        let mut context = Context::new(*b"HOST", 9300);
        context.address = Some("192.168.1.50:9300".parse().unwrap());
        context.wifi_direct = Some(WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: "PacketLab-auto".into(),
            password: "autopassword".into(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 5180,
        });
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.remote = Some("PEER".into());
        upgrade.context = Some(context.clone());

        // 1. First offer is WifiLan with a fast probe deadline (~3.5s)
        let lan_frame = upgrade.offer().await.unwrap().unwrap();
        let lan_path = lan_frame
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap()
            .upgrade_path_info
            .unwrap();
        assert_eq!(lan_path.medium(), Medium::WifiLan);
        assert!(upgrade.deadline.unwrap() <= tokio::time::Instant::now() + Duration::from_secs(4));

        // 2. When LAN probe deadline expires (e.g. phone is not on the same Wi-Fi or AP isolated),
        // on_deadline_expired() automatically returns the WifiHotspot offer instead of failing!
        let hotspot_frame = upgrade.on_deadline_expired().await.unwrap().unwrap();
        let hotspot_path = hotspot_frame
            .v1
            .unwrap()
            .bandwidth_upgrade_negotiation
            .unwrap()
            .upgrade_path_info
            .unwrap();
        assert_eq!(hotspot_path.medium(), Medium::WifiHotspot);
        let creds = hotspot_path.wifi_hotspot_credentials.as_ref().unwrap();
        assert_eq!(creds.ssid.as_deref(), Some("PacketLab-auto"));
        assert_eq!(creds.frequency, Some(5180));
        assert!(!upgrade.lan_failed && upgrade.lan_attempted && upgrade.waiting.is_some());
        clear_p2p_upgrader();
    }

    #[tokio::test]
    async fn unjoined_hotspot_timeout_releases_listener_for_bluetooth_fallback() {
        let mut context = Context::new(*b"HOST", 9300);
        context.address = None;
        context.wifi_direct = Some(WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: "PacketLab-timeout".into(),
            password: "testpassword".into(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 5180,
        });
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.remote = Some("PEER".into());
        upgrade.context = Some(context.clone());
        assert!(upgrade.offer().await.unwrap().is_some());
        assert!(upgrade.on_deadline_expired().await.unwrap().is_none());
        assert!(upgrade.waiting.is_none() && upgrade.deadline.is_none());
        assert!(upgrade.direct_failed && upgrade.lan_failed);
        assert!(context.pending.lock().unwrap().is_empty());
        assert_eq!(upgrade.medium_label(), "Bluetooth");
    }

    #[tokio::test]
    async fn glare_with_peer_wifi_direct_cancels_local_offer_and_replies_hotspot() {
        let _guard = TEST_P2P_MUTEX.lock().await;
        clear_p2p_upgrader();
        let mut context = Context::new(*b"HOST", 9300);
        context.address = Some("192.168.1.50:9300".parse().unwrap());
        context.wifi_direct = Some(WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: "PacketLab-glare".into(),
            password: "glarepassword".into(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 5180,
        });
        let mut upgrade = Upgrade::default();
        upgrade.bluetooth = true;
        upgrade.remote = Some("PEER".into());
        upgrade.context = Some(context.clone());

        // 1. Local side offers LAN and is waiting on its listener.
        let _local_offer = upgrade.offer().await.unwrap().unwrap();
        assert!(upgrade.waiting.is_some());

        // 2. Peer simultaneously sends UpgradePathAvailable(WifiDirect).
        let peer_negotiation = wire::BandwidthUpgradeNegotiationFrame {
            event_type: Some(EventType::UpgradePathAvailable.into()),
            upgrade_path_info: Some(bwu::UpgradePathInfo {
                medium: Some(Medium::WifiDirect.into()),
                wifi_direct_credentials: Some(bwu::upgrade_path_info::WifiDirectCredentials {
                    ssid: Some("DIRECT-peer-phone".into()),
                    password: Some("peerpassword123".into()),
                    port: Some(9300),
                    frequency: Some(5180),
                    gateway: Some("192.168.49.1".into()),
                    ..Default::default()
                }),
                supports_disabling_encryption: Some(false),
                supports_client_introduction_ack: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 3. Handle should resolve glare gracefully without bailing, reject peer's direct path, and offer local hotspot.
        let action = upgrade.handle(&peer_negotiation).await.unwrap();
        let Action::Send(replies) = action else {
            panic!("Expected Action::Send replies");
        };
        assert_eq!(replies.len(), 2);
        // First reply is UpgradeFailure for peer's path
        let first = replies[0].v1.as_ref().unwrap().bandwidth_upgrade_negotiation.as_ref().unwrap();
        assert_eq!(first.event_type(), EventType::UpgradeFailure);
        // Second reply is UpgradePathAvailable with local hotspot credentials
        let second = replies[1].v1.as_ref().unwrap().bandwidth_upgrade_negotiation.as_ref().unwrap();
        assert_eq!(second.event_type(), EventType::UpgradePathAvailable);
        let second_path = second.upgrade_path_info.as_ref().unwrap();
        assert_eq!(second_path.medium(), Medium::WifiHotspot);
        assert!(upgrade.waiting.is_some());
        assert!(upgrade.lan_failed);
        let deadline = upgrade.deadline;
        let action = upgrade.handle(&peer_negotiation).await.unwrap();
        assert!(matches!(action, Action::Send(ref replies) if replies.len() == 1));
        assert!(upgrade.waiting.is_some(), "A competing offer must not abandon our advertised hotspot");
        assert_eq!(upgrade.deadline, deadline);
        assert!(!upgrade.direct_failed);
        assert!(context.pending.lock().unwrap().contains_key("PEER"));
    }
}
