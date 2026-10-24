//! Wi-Fi Direct (P2P) device management via NetworkManager D-Bus.
#![allow(dead_code)]
use std::collections::HashMap;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{Connection, Result};

pub const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
pub const NM_INTERFACE: &str = "org.freedesktop.NetworkManager";
pub const NM_DEVICE_WIFI_P2P_INTERFACE: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
pub const NM_WIFI_P2P_PEER_INTERFACE: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";

#[zbus::proxy(
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager",
    interface = "org.freedesktop.NetworkManager"
)]
pub trait NetworkManager {
    fn get_device_by_ip_iface(&self, iface: &str) -> Result<OwnedObjectPath>;
    fn get_devices(&self) -> Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: OwnedObjectPath,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
    ) -> Result<OwnedObjectPath>;
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
    ) -> Result<(OwnedObjectPath, OwnedObjectPath)>;
    fn deactivate_connection(&self, active_connection: OwnedObjectPath) -> Result<()>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.NetworkManager",
    interface = "org.freedesktop.NetworkManager.Device.WifiP2P"
)]
pub trait WifiP2P {
    fn start_find(&self, options: HashMap<&str, Value<'_>>) -> Result<()>;
    fn stop_find(&self) -> Result<()>;

    #[zbus(property)]
    fn peers(&self) -> Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn hw_address(&self) -> Result<String>;

    #[zbus(signal)]
    fn peer_added(&self, peer: OwnedObjectPath) -> Result<()>;

    #[zbus(signal)]
    fn peer_removed(&self, peer: OwnedObjectPath) -> Result<()>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.NetworkManager",
    interface = "org.freedesktop.NetworkManager.WifiP2PPeer"
)]
pub trait WifiP2PPeer {
    #[zbus(property)]
    fn name(&self) -> Result<String>;

    #[zbus(property)]
    fn hw_address(&self) -> Result<String>;

    #[zbus(property)]
    fn strength(&self) -> Result<u8>;

    #[zbus(property)]
    fn manufacturer(&self) -> Result<String>;

    #[zbus(property)]
    fn model(&self) -> Result<String>;

    #[zbus(property)]
    fn serial(&self) -> Result<String>;
}

#[derive(Clone, Debug)]
pub struct P2PPeerInfo {
    pub path: OwnedObjectPath,
    pub name: String,
    pub hw_address: String,
    pub strength: u8,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct WifiP2PController {
    conn: Connection,
    device_path: OwnedObjectPath,
}

impl WifiP2PController {
    pub async fn new(conn: Connection) -> anyhow::Result<Self> {
        let nm_proxy = NetworkManagerProxy::new(&conn).await?;
        // First try to find p2p-dev-wlo1 directly
        let device_path = match nm_proxy.get_device_by_ip_iface("p2p-dev-wlo1").await {
            Ok(path) => path,
            Err(_) => {
                // Fallback: search all devices for one supporting Device.WifiP2P
                let devices = nm_proxy.get_devices().await?;
                let mut found = None;
                for dev in devices {
                    let dev_proxy = zbus::Proxy::new(
                        &conn,
                        NM_SERVICE,
                        dev.clone(),
                        "org.freedesktop.NetworkManager.Device",
                    )
                    .await?;
                    if let Ok(dev_type) = dev_proxy.get_property::<u32>("DeviceType").await {
                        // NM_DEVICE_TYPE_WIFI_P2P = 30
                        if dev_type == 30 {
                            found = Some(dev);
                            break;
                        }
                    }
                }
                found.ok_or_else(|| anyhow::anyhow!("No Wi-Fi P2P device found"))?
            }
        };

        tracing::info!(device_path = %device_path.as_str(), "Initialized Wi-Fi P2P controller");
        Ok(Self { conn, device_path })
    }

    pub fn device_path(&self) -> &OwnedObjectPath {
        &self.device_path
    }

    pub async fn start_find(&self) -> anyhow::Result<()> {
        let p2p_proxy = WifiP2PProxy::builder(&self.conn)
            .path(&self.device_path)?
            .build()
            .await?;
        p2p_proxy.start_find(HashMap::new()).await?;
        tracing::debug!("Started Wi-Fi P2P peer discovery");
        Ok(())
    }

    pub async fn stop_find(&self) -> anyhow::Result<()> {
        let p2p_proxy = WifiP2PProxy::builder(&self.conn)
            .path(&self.device_path)?
            .build()
            .await?;
        p2p_proxy.stop_find().await?;
        tracing::debug!("Stopped Wi-Fi P2P peer discovery");
        Ok(())
    }

    pub async fn list_peers(&self) -> anyhow::Result<Vec<P2PPeerInfo>> {
        let p2p_proxy = WifiP2PProxy::builder(&self.conn)
            .path(&self.device_path)?
            .build()
            .await?;
        let peer_paths = p2p_proxy.peers().await?;
        let mut result = Vec::new();

        for path in peer_paths {
            if let Ok(peer_proxy) = WifiP2PPeerProxy::builder(&self.conn).path(&path) {
                if let Ok(proxy) = peer_proxy.build().await {
                    let name = proxy.name().await.unwrap_or_default();
                    let hw_address = proxy.hw_address().await.unwrap_or_default();
                    let strength = proxy.strength().await.unwrap_or(0);
                    let model = proxy.model().await.unwrap_or_default();
                    result.push(P2PPeerInfo {
                        path,
                        name,
                        hw_address,
                        strength,
                        model,
                    });
                }
            }
        }

        Ok(result)
    }

    pub async fn connect_peer(
        &self,
        peer_path: &OwnedObjectPath,
        peer_hw_address: &str,
    ) -> anyhow::Result<(OwnedObjectPath, OwnedObjectPath)> {
        let nm_proxy = NetworkManagerProxy::new(&self.conn).await?;
        let mut conn_settings: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();

        let mut connection_map = HashMap::new();
        connection_map.insert("type", Value::from("wifi-p2p"));
        connection_map.insert("id", Value::from("PacketLab-P2P"));
        connection_map.insert("autoconnect", Value::from(false));
        conn_settings.insert("connection", connection_map);

        let mut p2p_map = HashMap::new();
        p2p_map.insert("peer", Value::from(peer_hw_address));
        conn_settings.insert("wifi-p2p", p2p_map);

        let (con_path, active_con_path) = nm_proxy
            .add_and_activate_connection(
                conn_settings,
                self.device_path.clone(),
                peer_path.clone(),
            )
            .await?;
        tracing::info!(
            connection = %con_path.as_str(),
            active_connection = %active_con_path.as_str(),
            "Initiated Wi-Fi P2P connection to peer"
        );
        Ok((con_path, active_con_path))
    }

    pub async fn disconnect(&self, active_conn_path: &OwnedObjectPath) -> anyhow::Result<()> {
        let nm_proxy = NetworkManagerProxy::new(&self.conn).await?;
        nm_proxy.deactivate_connection(active_conn_path.clone()).await?;
        tracing::info!(active_connection = %active_conn_path.as_str(), "Deactivated P2P connection");
        Ok(())
    }

    pub async fn connect_for_upgrade(
        &self,
        req: &rqs_lib::P2pUpgradeRequest,
    ) -> anyhow::Result<()> {
        self.start_find().await?;
        let ssid_hint = req
            .ssid
            .strip_prefix("DIRECT-")
            .and_then(|rest| rest.split_once('-').map(|(_, tail)| tail))
            .unwrap_or(&req.ssid);
        for _ in 0..6 {
            if let Ok(peers) = self.list_peers().await {
                if let Some(peer) = peers
                    .iter()
                    .find(|p| {
                        !ssid_hint.is_empty()
                            && (p.name.contains(ssid_hint) || req.ssid.contains(&p.name))
                    })
                    .or_else(|| peers.iter().max_by_key(|p| p.strength))
                {
                    tracing::info!(
                        peer_name = %peer.name,
                        peer_mac = %peer.hw_address,
                        ssid = %req.ssid,
                        "Activating NetworkManager Device.WifiP2P peer for Quick Share upgrade"
                    );
                    let _ = self.connect_peer(&peer.path, &peer.hw_address).await?;
                    let _ = self.stop_find().await;
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        }
        let _ = self.stop_find().await;
        anyhow::bail!("No matching Wi-Fi P2P peer found for SSID {}", req.ssid)
    }
}

pub fn register_with_rqs() {
    rqs_lib::set_p2p_upgrader(std::sync::Arc::new(|req| {
        Box::pin(async move {
            let conn = Connection::system().await?;
            let ctrl = WifiP2PController::new(conn).await?;
            ctrl.connect_for_upgrade(&req).await
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_p2p_controller_init() {
        if let Ok(conn) = Connection::system().await {
            match WifiP2PController::new(conn).await {
                Ok(ctrl) => {
                    println!("Found P2P device: {:?}", ctrl.device_path());
                    assert!(!ctrl.device_path().as_str().is_empty());
                    assert!(ctrl.device_path().as_str().starts_with("/org/freedesktop/NetworkManager/Devices/"));

                    if let Ok(peers) = ctrl.list_peers().await {
                        println!("Current P2P peers: {peers:?}");
                    }
                }
                Err(e) => {
                    println!("No P2P device or error: {e}");
                }
            }
        }
    }
}
