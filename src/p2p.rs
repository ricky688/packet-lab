//! Wi-Fi Direct (P2P) device management via NetworkManager D-Bus.
#![allow(dead_code)]
use rqs_lib::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
use anyhow::Context;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
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
    fn add_and_activate_connection2(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
        options: HashMap<&str, Value<'_>>,
    ) -> Result<(
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    )>;
    fn deactivate_connection(&self, active_connection: OwnedObjectPath) -> Result<()>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.NetworkManager",
    interface = "org.freedesktop.NetworkManager.Settings.Connection"
)]
pub trait ConnectionSettings {
    fn delete(&self) -> Result<()>;
}

#[derive(Debug)]
pub struct ActiveWifiDirectGroup {
    pub connection_path: OwnedObjectPath,
    pub active_connection_path: OwnedObjectPath,
    pub ssid: String,
    pub password: String,
    pub gateway: Ipv4Addr,
    pub port: u16,
    pub frequency: i32,
}

static GROUP_OPERATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct DirectGroup {
    child: std::process::Child,
    offer: rqs_lib::WifiDirectOffer,
}
static DIRECT_GROUP: Mutex<Option<DirectGroup>> = Mutex::new(None);
static P2P_CLIENT: Mutex<Option<(OwnedObjectPath, OwnedObjectPath)>> = Mutex::new(None);
static LEGACY_CLIENT: Mutex<Option<std::process::Child>> = Mutex::new(None);

fn root_owned_helper(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.is_file() && path.ancestors().all(|part| {
        part.symlink_metadata().is_ok_and(|metadata| {
            metadata.uid() == 0 && metadata.mode() & 0o022 == 0
                && !metadata.file_type().is_symlink()
        })
    })
}

fn network_helper_command(name: &str) -> std::process::Command {
    let system_helper = std::path::Path::new("/usr/lib/packet-lab-network").join(name);
    let mut command = std::process::Command::new("/usr/bin/pkexec");
    command.arg("--disable-internal-agent");
    if root_owned_helper(&system_helper) {
        tracing::info!(helper = name, "Using installed Packet Lab network authorization");
        command.arg(system_helper);
    } else {
        let bundled = std::env::current_exe().ok().and_then(|exe| {
            Some(exe.parent()?.parent()?.join("share/packet-lab").join(name))
        }).filter(|path| path.is_file());
        let helper = bundled.unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts").join(name)
        });
        command.args(["/usr/bin/python3", "-I"]).arg(helper);
    }
    command
}

async fn connect_legacy_client(parent: String, req: rqs_lib::P2pUpgradeRequest) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, Write};
        let mut child = network_helper_command("wifi-p2p-client-helper.py")
            .arg(parent)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
        let hex = |value: &str| value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        // Credentials travel in a pipe, never argv or application logs.
        writeln!(child.stdin.as_mut().unwrap(), "{}\t{}\t{}\t{}",
            hex(&req.ssid), hex(&req.password), req.gateway, req.frequency.unwrap_or(0))?;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
        if !line.starts_with("READY\tp2p-") {
            drop(child.stdin.take());
            let _ = child.wait();
            anyhow::bail!("Legacy Wi-Fi Direct client did not become ready; check system authorization and helper log");
        }
        tracing::info!(interface = %line.trim().trim_start_matches("READY\t"),
            "Legacy Wi-Fi Direct P2P-client associated and DHCP ready; station/VPN retained");
        *LEGACY_CLIENT.lock().unwrap() = Some(child);
        Ok(())
    }).await?
}

fn matches_p2p_peer(name: &str, device_name: &str) -> bool {
    !device_name.is_empty() && name == device_name
}

fn has_gateway_subnet(addresses: &[Vec<u32>], gateway: Ipv4Addr) -> bool {
    addresses.iter().any(|address| {
        if address.len() < 2 || !(1..=32).contains(&address[1]) { return false; }
        let local = Ipv4Addr::from(address[0].to_ne_bytes());
        let mask = u32::MAX << (32 - address[1]);
        local != gateway && (u32::from(local) & mask) == (u32::from(gateway) & mask)
    })
}

fn channel_for_frequency(frequency: u32) -> anyhow::Result<(&'static str, u32)> {
    match frequency {
        2412..=2472 if (frequency - 2407) % 5 == 0 => Ok(("bg", (frequency - 2407) / 5)),
        5180..=5885 if (frequency - 5000) % 5 == 0 => Ok(("a", (frequency - 5000) / 5)),
        _ => anyhow::bail!("Unsupported concurrent AP frequency {frequency}"),
    }
}

async fn create_direct_group(
    parent: String,
    frequency: u32,
    port: u16,
) -> anyhow::Result<rqs_lib::WifiDirectOffer> {
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let mut child = network_helper_command("wifi-ap-helper.py")
            .arg(parent)
            .arg(frequency.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
        let fields: Vec<_> = line.trim_end().split('\t').collect();
        if fields.len() != 4 {
            drop(child.stdin.take());
            let _ = child.wait();
            anyhow::bail!(
                "Wi-Fi AP helper did not return ready credentials; check system authorization"
            );
        }
        let offer = rqs_lib::WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: fields[1].to_owned(),
            password: fields[2].to_owned(),
            gateway: Ipv4Addr::new(192, 168, 49, 1),
            port,
            frequency: fields[3].parse()?,
        };
        *DIRECT_GROUP.lock().unwrap() = Some(DirectGroup {
            child,
            offer: offer.clone(),
        });
        tracing::info!(frequency, "Concurrent isolated AP and DHCP ready");
        Ok::<_, anyhow::Error>(offer)
    })
    .await?
}

async fn hotspot_device(
    conn: &Connection,
    nm: &NetworkManagerProxy<'_>,
) -> anyhow::Result<(Option<OwnedObjectPath>, String, u32)> {
    let mut idle = None;
    let mut connected = None;
    for path in nm.get_devices().await? {
        let device = zbus::Proxy::new(
            conn,
            NM_SERVICE,
            path.clone(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await?;
        if device.get_property::<u32>("DeviceType").await? != 2 {
            continue;
        }
        let iface = device.get_property::<String>("Interface").await?;
        if iface == "plabap0" {
            continue;
        }
        let state = device.get_property::<u32>("State").await?;
        if state == 30 && idle.is_none() {
            idle = Some((path.clone(), iface.clone()));
        }
        if state == 100 {
            let wireless = zbus::Proxy::new(
                conn,
                NM_SERVICE,
                path.clone(),
                "org.freedesktop.NetworkManager.Device.Wireless",
            )
            .await?;
            let ap_path = wireless
                .get_property::<OwnedObjectPath>("ActiveAccessPoint")
                .await?;
            let ap = zbus::Proxy::new(
                conn,
                NM_SERVICE,
                ap_path,
                "org.freedesktop.NetworkManager.AccessPoint",
            )
            .await?;
            let frequency = ap.get_property::<u32>("Frequency").await?;
            channel_for_frequency(frequency)?;
            connected = Some((iface, frequency));
        }
    }
    if let Some((parent, frequency)) = connected {
        return Ok((None, parent, frequency));
    }
    if let Some((_path, iface)) = idle {
        return Ok((None, iface, 5180));
    }
    anyhow::bail!("No usable Wi-Fi adapter")
}

static ACTIVE_P2P_GROUP: Mutex<Option<ActiveWifiDirectGroup>> = Mutex::new(None);

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
        p2p_map.insert("wps-method", Value::from(4u32));
        conn_settings.insert("wifi-p2p", p2p_map);
        conn_settings.insert("ipv4", HashMap::from([
            ("method", Value::from("auto")), ("never-default", Value::from(true)),
            ("ignore-auto-dns", Value::from(true)),
        ]));
        conn_settings.insert("ipv6", HashMap::from([("method", Value::from("disabled"))]));

        let (con_path, active_con_path, _) = nm_proxy
            .add_and_activate_connection2(conn_settings, self.device_path.clone(), peer_path.clone(),
                HashMap::from([("persist", Value::from("volatile"))]))
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
        nm_proxy
            .deactivate_connection(active_conn_path.clone())
            .await?;
        tracing::info!(active_connection = %active_conn_path.as_str(), "Deactivated P2P connection");
        Ok(())
    }

    pub async fn connect_for_upgrade(
        &self,
        req: &rqs_lib::P2pUpgradeRequest,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(req.medium == Medium::WifiDirect,
            "A peer Wi-Fi Hotspot requires a station connection; P2P discovery cannot join it while preserving the active Wi-Fi/VPN");
        let _operation = GROUP_OPERATION.lock().await;
        anyhow::ensure!(P2P_CLIENT.lock().unwrap().is_none() && LEGACY_CLIENT.lock().unwrap().is_none(),
            "A P2P transfer is already active");
        if req.device_name.is_none() {
            let device = zbus::Proxy::new(&self.conn, NM_SERVICE, self.device_path.clone(),
                "org.freedesktop.NetworkManager.Device").await?;
            let name = device.get_property::<String>("Interface").await?;
            let parent = name.strip_prefix("p2p-dev-").context("Unknown P2P parent interface")?;
            return connect_legacy_client(parent.to_owned(), req.clone()).await;
        }
        let device_name = req.device_name.as_deref().unwrap();
        self.start_find().await?;
        for _ in 0..30 {
            if let Ok(peers) = self.list_peers().await {
                if let Some(peer) = peers
                    .iter()
                    .find(|p| matches_p2p_peer(&p.name, device_name))
                {
                    tracing::info!(
                        peer_name = %peer.name,
                        peer_mac = %peer.hw_address,
                        ssid = %req.ssid,
                        "Activating NetworkManager Device.WifiP2P peer for Quick Share upgrade"
                    );
                    let activation = self.connect_peer(&peer.path, &peer.hw_address).await;
                    let _ = self.stop_find().await;
                    let (profile, active_path) = activation?;
                    let ready = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        let active = zbus::Proxy::new(&self.conn, NM_SERVICE, active_path.clone(),
                            "org.freedesktop.NetworkManager.Connection.Active").await?;
                        loop {
                            match active.get_property::<u32>("State").await? {
                                2 => {
                                    let ip = active.get_property::<OwnedObjectPath>("Ip4Config").await?;
                                    if ip.as_str() != "/" {
                                        let config = zbus::Proxy::new(&self.conn, NM_SERVICE, ip,
                                            "org.freedesktop.NetworkManager.IP4Config").await?;
                                        let addresses = config.get_property::<Vec<Vec<u32>>>("Addresses").await?;
                                        if has_gateway_subnet(&addresses, req.gateway) {
                                            return Ok::<_, anyhow::Error>(());
                                        }
                                    }
                                }
                                3 | 4 => anyhow::bail!("Wi-Fi P2P activation failed"),
                                _ => {}
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }).await;
                    if !matches!(ready, Ok(Ok(()))) {
                        let _ = self.disconnect(&active_path).await;
                        if let Ok(proxy) = ConnectionSettingsProxy::builder(&self.conn).path(&profile)?.build().await {
                            let _ = proxy.delete().await;
                        }
                        anyhow::bail!("Wi-Fi P2P link/address did not become ready: {ready:?}");
                    }
                    *P2P_CLIENT.lock().unwrap() = Some((profile, active_path));
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        }
        let _ = self.stop_find().await;
        anyhow::bail!("No matching Wi-Fi P2P peer found for device name {device_name}")
    }
}

pub async fn create_transient_wifi_direct_group(
    port: u16,
) -> anyhow::Result<rqs_lib::WifiDirectOffer> {
    anyhow::ensure!(port != 0, "Hotspot requires a nonzero listener port");
    let _operation = GROUP_OPERATION.lock().await;
    {
        let mut group = DIRECT_GROUP.lock().unwrap();
        if let Some(active) = group.as_mut() {
            if active.child.try_wait()?.is_none() {
                if let Some(stdin) = active.child.stdin.as_mut() {
                    let _ = std::io::Write::write_all(stdin, b"\n");
                }
                let mut offer = active.offer.clone();
                offer.port = port;
                return Ok(offer);
            }
            *group = None;
        }
    }
    if let Some(existing) = ACTIVE_P2P_GROUP.lock().unwrap().as_ref() {
        return Ok(rqs_lib::WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: existing.ssid.clone(),
            password: existing.password.clone(),
            gateway: existing.gateway,
            port,
            frequency: existing.frequency,
        });
    }

    let conn = Connection::system().await?;
    let nm_proxy = NetworkManagerProxy::new(&conn).await?;

    let mut stations = Vec::new();
    for path in nm_proxy.get_devices().await? {
        let device = zbus::Proxy::new(
            &conn,
            NM_SERVICE,
            path.clone(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await?;
        if device.get_property::<u32>("DeviceType").await? == 2
            && device.get_property::<u32>("State").await? == 100
        {
            stations.push((
                path,
                device
                    .get_property::<OwnedObjectPath>("ActiveConnection")
                    .await?,
            ));
        }
    }
    let (wifi_dev, parent, frequency) = hotspot_device(&conn, &nm_proxy).await?;
    let Some(wifi_dev) = wifi_dev else {
        return create_direct_group(parent, frequency, port).await;
    };
    let (band, channel) = channel_for_frequency(frequency)?;

    // UUID randomness supplies a fresh WPA2 key for each transient hotspot.
    let uuid_str = gtk::glib::uuid_string_random().to_string().replace('-', "");
    let suffix = &uuid_str[..4];
    let ssid = format!("PacketLab-{}", suffix);

    let password = uuid_str.clone();

    let mut conn_settings = HashMap::new();

    let mut connection_map = HashMap::new();
    connection_map.insert("id", Value::from("PacketLab-Hotspot"));
    connection_map.insert("type", Value::from("802-11-wireless"));
    connection_map.insert("autoconnect", Value::from(false));
    conn_settings.insert("connection", connection_map);

    let mut wireless_map = HashMap::new();
    wireless_map.insert("mode", Value::from("ap"));
    let ssid_bytes: Vec<u8> = ssid.as_bytes().to_vec();
    wireless_map.insert("ssid", Value::from(ssid_bytes));
    wireless_map.insert("band", Value::from(band));
    wireless_map.insert("channel", Value::from(channel));
    conn_settings.insert("802-11-wireless", wireless_map);

    let mut sec_map = HashMap::new();
    sec_map.insert("key-mgmt", Value::from("wpa-psk"));
    sec_map.insert("psk", Value::from(password.clone()));
    conn_settings.insert("802-11-wireless-security", sec_map);

    let mut ipv4_map = HashMap::new();
    ipv4_map.insert("method", Value::from("shared"));
    // 192.168.49.1 in little endian u32: 20031680
    let ip_int = u32::from_le_bytes(Ipv4Addr::new(192, 168, 49, 1).octets());
    let addr_tuple = vec![ip_int, 24, 0];
    let addresses_val = vec![addr_tuple];
    ipv4_map.insert("addresses", Value::from(addresses_val));
    conn_settings.insert("ipv4", ipv4_map);

    let mut ipv6_map = HashMap::new();
    ipv6_map.insert("method", Value::from("ignore"));
    conn_settings.insert("ipv6", ipv6_map);

    tracing::info!(ssid = %ssid, "Creating transient Wi-Fi hotspot for Quick Share handover");

    let specific_obj = OwnedObjectPath::try_from("/")?;
    let (conn_path, active_conn_path, _) = nm_proxy
        .add_and_activate_connection2(
            conn_settings,
            wifi_dev.clone(),
            specific_obj,
            HashMap::from([("persist", Value::from("volatile"))]),
        )
        .await?;

    let ready = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let active = zbus::Proxy::new(
            &conn,
            NM_SERVICE,
            active_conn_path.clone(),
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .await?;
        loop {
            match active.get_property::<u32>("State").await? {
                2 => break,
                3 | 4 => anyhow::bail!("Hotspot activation failed"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        let device = zbus::Proxy::new(
            &conn,
            NM_SERVICE,
            wifi_dev,
            "org.freedesktop.NetworkManager.Device.Wireless",
        )
        .await?;
        let ap_path = device
            .get_property::<OwnedObjectPath>("ActiveAccessPoint")
            .await?;
        let ap = zbus::Proxy::new(
            &conn,
            NM_SERVICE,
            ap_path,
            "org.freedesktop.NetworkManager.AccessPoint",
        )
        .await?;
        let actual_frequency = ap.get_property::<u32>("Frequency").await?;
        anyhow::ensure!(
            actual_frequency == frequency,
            "Concurrent hotspot frequency changed: {actual_frequency} != {frequency}"
        );
        for (path, original) in &stations {
            let station = zbus::Proxy::new(
                &conn,
                NM_SERVICE,
                path.clone(),
                "org.freedesktop.NetworkManager.Device",
            )
            .await?;
            anyhow::ensure!(
                station.get_property::<u32>("State").await? == 100
                    && station
                        .get_property::<OwnedObjectPath>("ActiveConnection")
                        .await?
                        == *original,
                "Concurrent AP changed the existing Wi-Fi connection; rolling back"
            );
        }
        let listener =
            tokio::net::TcpStream::connect((Ipv4Addr::new(192, 168, 49, 1), port)).await?;
        drop(listener);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    if !matches!(ready, Ok(Ok(()))) {
        let _ = nm_proxy
            .deactivate_connection(active_conn_path.clone())
            .await;
        if let Ok(proxy) = ConnectionSettingsProxy::builder(&conn)
            .path(&conn_path)?
            .build()
            .await
        {
            let _ = proxy.delete().await;
        }
        anyhow::bail!("Hotspot did not become ready: {ready:?}");
    }
    tracing::info!(frequency, "Transient Wi-Fi hotspot ready on 192.168.49.1");

    let gateway = Ipv4Addr::new(192, 168, 49, 1);

    let offer = rqs_lib::WifiDirectOffer {
        medium: Medium::WifiHotspot,
        ssid: ssid.clone(),
        password: password.clone(),
        gateway,
        port,
        frequency: frequency as i32,
    };

    *ACTIVE_P2P_GROUP.lock().unwrap() = Some(ActiveWifiDirectGroup {
        connection_path: conn_path,
        active_connection_path: active_conn_path,
        ssid,
        password,
        gateway,
        port,
        frequency: frequency as i32,
    });

    Ok(offer)
}

pub async fn cleanup_wifi_direct_group() {
    let _operation = GROUP_OPERATION.lock().await;
    let legacy = LEGACY_CLIENT.lock().unwrap().take();
    if let Some(mut child) = legacy {
        drop(child.stdin.take());
        let _ = tokio::task::spawn_blocking(move || child.wait()).await;
    }
    let client = P2P_CLIENT.lock().unwrap().take();
    if let Some((profile, active)) = client {
        if let Ok(conn) = Connection::system().await {
            if let Ok(nm) = NetworkManagerProxy::new(&conn).await {
                let _ = nm.deactivate_connection(active).await;
            }
            if let Ok(builder) = ConnectionSettingsProxy::builder(&conn).path(&profile) {
                if let Ok(settings) = builder.build().await { let _ = settings.delete().await; }
            }
        }
    }
    let group = ACTIVE_P2P_GROUP.lock().unwrap().take();
    if let Some(group) = group {
        tracing::info!(
            connection = %group.connection_path.as_str(),
            active = %group.active_connection_path.as_str(),
            "Cleaning up Wi-Fi hotspot"
        );
        if let Ok(conn) = Connection::system().await {
            if let Ok(nm_proxy) = NetworkManagerProxy::new(&conn).await {
                let _ = nm_proxy
                    .deactivate_connection(group.active_connection_path)
                    .await;
            }
            if let Ok(settings_proxy) =
                ConnectionSettingsProxy::builder(&conn).path(&group.connection_path)
            {
                if let Ok(proxy) = settings_proxy.build().await {
                    let _ = proxy.delete().await;
                }
            }
        }
        tracing::info!("Wi-Fi hotspot cleaned up");
    }
    let direct = DIRECT_GROUP.lock().unwrap().take();
    if let Some(mut direct) = direct {
        drop(direct.child.stdin.take());
        let _ = tokio::task::spawn_blocking(move || direct.child.wait()).await;
    }
}

pub async fn detect_local_p2p_offer(port: u16) -> Option<rqs_lib::WifiDirectOffer> {
    if let Ok(raw) = std::env::var("RQS_P2P_GO") {
        let parts: Vec<&str> = raw.splitn(4, ':').collect();
        if parts.len() == 4 {
            let ssid = parts[0].trim().to_owned();
            let password = parts[1].to_owned();
            let gateway = parts[2].trim().parse().ok()?;
            let frequency = parts[3].trim().parse().ok()?;
            if !ssid.is_empty() && !password.is_empty() {
                return Some(rqs_lib::WifiDirectOffer {
                    medium: Medium::WifiDirect,
                    ssid,
                    password,
                    gateway,
                    port,
                    frequency,
                });
            }
        }
    }

    // Dynamic Wi-Fi Direct / AP creation:
    // If we already have an active group, return it immediately.
    {
        let mut group = DIRECT_GROUP.lock().unwrap();
        if let Some(active) = group.as_mut() {
            if active.child.try_wait().ok().flatten().is_none() {
                if let Some(stdin) = active.child.stdin.as_mut() {
                    let _ = std::io::Write::write_all(stdin, b"\n");
                }
                let mut offer = active.offer.clone();
                offer.port = port;
                return Some(offer);
            }
            *group = None;
        }
    }
    if let Some(existing) = ACTIVE_P2P_GROUP.lock().unwrap().as_ref() {
        return Some(rqs_lib::WifiDirectOffer {
            medium: Medium::WifiHotspot,
            ssid: existing.ssid.clone(),
            password: existing.password.clone(),
            gateway: existing.gateway,
            port,
            frequency: existing.frequency,
        });
    }

    // Keep network activation asynchronous on the transfer runtime.
    tracing::info!("Invoking dynamic Wi-Fi hotspot creation for LAN fallback");
    match create_transient_wifi_direct_group(port).await {
        Ok(offer) => Some(offer),
        Err(err) => {
            tracing::error!("Failed to create dynamic Wi-Fi hotspot: {err:#}");
            cleanup_wifi_direct_group().await;
            None
        }
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
    rqs_lib::set_p2p_host_provider(std::sync::Arc::new(|port| {
        Box::pin(detect_local_p2p_offer(port))
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_helper_rejects_user_controlled_and_missing_files() {
        assert!(root_owned_helper(std::path::Path::new("/usr/bin/true")));
        assert!(!root_owned_helper(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/wifi-ap-helper.py").as_path()));
        assert!(!root_owned_helper(std::path::Path::new("/usr/lib/packet-lab-network/absent")));
    }

    #[test]
    fn p2p_selection_does_not_accept_an_unrelated_or_empty_name() {
        assert!(matches_p2p_peer("Phone", "Phone"));
        assert!(!matches_p2p_peer("Phone", "DIRECT-ab-Phone"));
        assert!(matches_p2p_peer("DIRECT-ab-Phone", "DIRECT-ab-Phone"));
        for name in ["", "Ph", "Other Phone", "Unrelated"] {
            assert!(!matches_p2p_peer(name, "DIRECT-ab-Phone"));
        }
    }

    #[test]
    fn p2p_readiness_requires_a_peer_subnet_address() {
        let address = |ip: Ipv4Addr, prefix| vec![u32::from_ne_bytes(ip.octets()), prefix, 0];
        let gateway = Ipv4Addr::new(192, 168, 49, 1);
        assert!(has_gateway_subnet(&[address(Ipv4Addr::new(192, 168, 49, 2), 24)], gateway));
        for addresses in [vec![], vec![vec![1]], vec![address(gateway, 24)],
            vec![address(Ipv4Addr::new(10, 14, 0, 2), 16)],
            vec![address(Ipv4Addr::new(192, 168, 49, 2), 0)],
            vec![address(Ipv4Addr::new(192, 168, 49, 2), 33)]] {
            assert!(!has_gateway_subnet(&addresses, gateway));
        }
    }

    #[test]
    fn concurrent_ap_uses_the_station_channel() {
        assert_eq!(channel_for_frequency(5745).unwrap(), ("a", 149));
        assert_eq!(channel_for_frequency(5180).unwrap(), ("a", 36));
        assert_eq!(channel_for_frequency(2462).unwrap(), ("bg", 11));
        for invalid in [0, 2413, 2484, 5181, 5955] {
            assert!(channel_for_frequency(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn test_p2p_controller_init() {
        if let Ok(conn) = Connection::system().await {
            match WifiP2PController::new(conn).await {
                Ok(ctrl) => {
                    println!("Found P2P device: {:?}", ctrl.device_path());
                    assert!(!ctrl.device_path().as_str().is_empty());
                    assert!(
                        ctrl.device_path()
                            .as_str()
                            .starts_with("/org/freedesktop/NetworkManager/Devices/")
                    );

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
