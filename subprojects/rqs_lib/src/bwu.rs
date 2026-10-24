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
        let address = get_if_addrs::get_if_addrs().ok().and_then(|interfaces| {
            interfaces.into_iter().find_map(|i| {
                let eligible = interface.as_ref().map_or_else(
                    || {
                        i.name.starts_with("wl")
                            || i.name.starts_with("en")
                            || i.name.starts_with("eth")
                    },
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
    pub fn offer(&mut self) -> anyhow::Result<Option<wire::OfflineFrame>> {
        if !self.bluetooth || self.complete || self.waiting.is_some() || self.next.is_some() {
            return Ok(None);
        }
        let context = self.context.as_ref().context("Missing upgrade context")?;
        let address = context
            .address
            .context("No physical Wi-Fi/Ethernet IPv4 address for handover")?;
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
                if path.medium() != Medium::WifiLan || path.supports_disabling_encryption() {
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
                let address = lan_address(
                    path.wifi_lan_socket
                        .as_ref()
                        .context("Missing Wi-Fi LAN socket")?,
                )?;
                let mut socket =
                    Transport::new(Connector::from_env()?.connect(&address.to_string()).await
                        .with_context(|| format!("Wi-Fi handover to {address} failed. Check local network access; public Wi-Fi may isolate devices"))?);
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
            EventType::UpgradeFailure => bail!("Peer rejected Wi-Fi upgrade"),
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
}
