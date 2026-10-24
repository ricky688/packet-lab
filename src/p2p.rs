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
                }
                Err(e) => {
                    println!("No P2P device or error: {e}");
                }
            }
        }
    }
}
