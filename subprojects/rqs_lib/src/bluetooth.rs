use crate::{
    DeviceType,
    hdl::{EndpointInfo, Visibility},
    transport::Transport,
    utils::parse_mdns_endpoint_info,
};
use anyhow::{Context, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bluer::{
    AdapterEvent, Address, AddressType,
    rfcomm::{Profile, Stream},
};
use futures::{StreamExt, pin_mut};
use std::{collections::HashMap, os::fd::AsRawFd, sync::{Arc, LazyLock}, time::Duration};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{broadcast, mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

static OUTGOING_CONNECTS: LazyLock<watch::Sender<usize>> = LazyLock::new(|| watch::channel(0).0);
static ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn outgoing_connections() -> watch::Receiver<usize> {
    OUTGOING_CONNECTS.subscribe()
}

pub(crate) struct DiscoveryScanGuard;

impl DiscoveryScanGuard {
    pub(crate) fn start() -> Option<Self> {
        let outgoing = OUTGOING_CONNECTS.borrow();
        if *outgoing != 0 {
            return None;
        }
        ACTIVE_SCANS.fetch_add(1, Ordering::SeqCst);
        Some(Self)
    }
}

impl Drop for DiscoveryScanGuard {
    fn drop(&mut self) {
        ACTIVE_SCANS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct OutgoingConnectGuard;

impl OutgoingConnectGuard {
    pub(crate) fn new() -> Self {
        OUTGOING_CONNECTS.send_modify(|count| *count += 1);
        Self
    }

    async fn pause_discovery(adapter: &bluer::Adapter) -> Self {
        let guard = Self::new();
        let paused = tokio::time::timeout(Duration::from_millis(1200), async {
            while ACTIVE_SCANS.load(Ordering::SeqCst) != 0 || adapter.is_discovering().await? {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            Ok::<_, bluer::Error>(())
        }).await;
        match paused {
            Ok(Ok(())) => debug!("Bluetooth adapter stopped discovery for outgoing connection"),
            Ok(Err(error)) => debug!("Could not check paused Bluetooth discovery: {error}"),
            Err(_) => debug!("Bluetooth adapter still discovering after outgoing pause window"),
        }
        guard
    }
}

impl Drop for OutgoingConnectGuard {
    fn drop(&mut self) {
        OUTGOING_CONNECTS.send_modify(|count| *count = count.saturating_sub(1));
    }
}

async fn wait_bt_connected(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // bluer::sock_priv::connect_priv calls AsyncFd::writable() right after non-blocking connect(),
    // which returns immediately from Tokio AsyncFd's initial speculative Ready::WRITABLE before
    // the kernel finishes the Bluetooth handshake (leaving the socket in BT_CONNECT -> ENOTCONN).
    // Poll the kernel directly until bt_sock_poll reports POLLOUT (BT_CONNECTED) or an error.
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc > 0 {
            let mut so_err: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut so_err as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if so_err != 0 {
                return Err(std::io::Error::from_raw_os_error(so_err));
            }
            if (pfd.revents & (libc::POLLERR | libc::POLLHUP)) != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "Bluetooth connection closed during handshake",
                ));
            }
            if (pfd.revents & libc::POLLOUT) != 0 {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Nearby's Java nameUUIDFromBytes("NearbySharing"), without a namespace prefix.
const SERVICE_UUID: Uuid = uuid!("a82efa21-ae5c-3dde-9bbc-f16da7b16c5a");
const FEF3_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");
const SERVICE_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const L2CAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
pub(crate) const L2CAP_SCHEME: &str = "ble-l2cap://";
const RFCOMM_REUSE_AGE: Duration = Duration::from_secs(30);
const RFCOMM_REUSE_TIMEOUT: Duration = Duration::from_millis(1500);

pub(crate) struct Connect {
    pub address: Address,
    pub endpoint: Option<[u8; 4]>,
    pub reply: oneshot::Sender<anyhow::Result<Transport>>,
}

struct RecentChannel {
    endpoint: [u8; 4],
    channel: u8,
    verified: tokio::time::Instant,
}

pub(crate) fn device_name(endpoint: [u8; 4], info: &[u8]) -> anyhow::Result<String> {
    if info.len() > 131 {
        bail!("Bluetooth endpoint info exceeds Nearby's limit");
    }
    let mut data = vec![0x23]; // Version 1, P2P_POINT_TO_POINT.
    data.extend_from_slice(&endpoint);
    data.extend_from_slice(&SERVICE_HASH);
    data.extend_from_slice(&[0; 7]);
    data.push(info.len() as u8);
    data.extend_from_slice(info);
    Ok(URL_SAFE_NO_PAD.encode(data))
}

fn parse_name(name: &str) -> anyhow::Result<([u8; 4], DeviceType, String)> {
    let data = URL_SAFE_NO_PAD.decode(name.trim_end_matches('='))?;
    if data.len() < 16 || data[0] != 0x23 || data[5..8] != SERVICE_HASH {
        bail!("Not a Nearby Sharing Bluetooth endpoint");
    }
    let end = 16 + usize::from(data[15]);
    if end > data.len() || data[15] > 131 {
        bail!("Truncated Bluetooth endpoint info");
    }
    let (kind, name) = parse_mdns_endpoint_info(&URL_SAFE_NO_PAD.encode(&data[16..end]))?;
    Ok((data[1..5].try_into()?, kind, name))
}

fn parse_receiving_payload(
    bytes: &[u8],
    fallback_address: Address,
) -> anyhow::Result<([u8; 4], DeviceType, String, Address)> {
    // Support both wrapped BLE v2 GATT slot payload (0x4x + SERVICE_HASH + len[4] + inner)
    // and direct BLE extended advertisement inner payload (0x23 + SERVICE_HASH + endpoint[4] + info_len + info + mac[6]).
    let inner = if bytes.len() >= 8 && (bytes[0] & 0xe0) == 0x40 && bytes[1..4] == SERVICE_HASH {
        let len = u32::from_be_bytes(bytes[4..8].try_into()?) as usize;
        let end = 8usize.checked_add(len).context("Invalid BLE slot length")?;
        if end > bytes.len() {
            bail!("Truncated BLE GATT slot wrapper");
        }
        &bytes[8..end]
    } else {
        bytes
    };

    if inner.len() < 9 || (inner[0] & 0xe0) != 0x20 || inner[1..4] != SERVICE_HASH {
        bail!("Not a Nearby Sharing BLE receiving payload");
    }
    let endpoint: [u8; 4] = inner[4..8].try_into()?;
    let info_len = usize::from(inner[8]);
    let info_end = 9 + info_len;
    if info_len > 131 || info_end > inner.len() {
        bail!("Truncated BLE endpoint info");
    }
    let (kind, name) = parse_mdns_endpoint_info(&URL_SAFE_NO_PAD.encode(&inner[9..info_end]))?;
    let has_classic_mac =
        inner.len() >= info_end + 6 && inner[info_end..info_end + 6] != [0; 6];
    let classic_address = if has_classic_mac {
        Address(inner[info_end..info_end + 6].try_into()?)
    } else {
        fallback_address
    };
    Ok((endpoint, kind, name, classic_address))
}

fn select_ble_outbound_route(
    id: &str,
    ble_address: Address,
    address_type: Option<AddressType>,
    psm: Option<u16>,
) -> String {
    if let Some(psm) = psm
        && let Some(address_type @ (AddressType::LePublic | AddressType::LeRandom)) = address_type
    {
        return format!("{L2CAP_SCHEME}{ble_address}/{address_type}/{psm}");
    }
    id.to_owned()
}

fn advertised_l2cap_psm(data: &[u8]) -> Option<u16> {
    if is_fef3_gatt_slot_header(data) {
        let psm = u16::from_be_bytes(data.get(15..17)?.try_into().ok()?);
        return (0x80..=0xff).contains(&psm).then_some(psm);
    }
    // Regular v2 advertisement: service hash, data length, data, two-byte token, extra fields.
    if data.get(..4)? != [0x48, SERVICE_HASH[0], SERVICE_HASH[1], SERVICE_HASH[2]] {
        return None;
    }
    let data_len = u32::from_be_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    let extra = data.get(data_len.checked_add(10)?..)?;
    if extra.first()? & 1 == 0 {
        return None;
    }
    let psm = u16::from_be_bytes(extra.get(1..3)?.try_into().ok()?);
    (0x80..=0xff).contains(&psm).then_some(psm)
}

fn is_fef3_gatt_slot_header(data: &[u8]) -> bool {
    // One version/flags byte, ten bloom-filter bytes, four hash bytes and an optional PSM.
    matches!(data.len(), 15 | 17) && data[0] & 0xe0 == 0x40 && data[0] & 0x0f != 0
}

type ReceivingEndpoint = ([u8; 4], DeviceType, String, Address);
type GattSlotCache = std::sync::Mutex<HashMap<Address, (Vec<u8>, ReceivingEndpoint)>>;

async fn cached_gatt_slot(
    cache: &GattSlotCache,
    address: Address,
    header: &[u8],
    read: impl std::future::Future<Output = anyhow::Result<Option<ReceivingEndpoint>>>,
) -> anyhow::Result<Option<ReceivingEndpoint>> {
    if let Some((previous, endpoint)) = cache.lock().unwrap().get(&address) {
        if previous == header {
            return Ok(Some(endpoint.clone()));
        }
    }
    let result = read.await?;
    if let Some(endpoint) = &result {
        cache.lock().unwrap().insert(address, (header.to_vec(), endpoint.clone()));
    }
    Ok(result)
}

async fn resolve_ble_fef3_device(
    device: &bluer::Device,
    address: Address,
    cache: &GattSlotCache,
) -> anyhow::Result<Option<([u8; 4], DeviceType, String, Address)>> {
    let service_data = device.service_data().await?.unwrap_or_default();
    let Some(data) = service_data.get(&FEF3_SERVICE_UUID) else {
        return Ok(None);
    };

    // 1. Try direct parsing (BLE 5.0 Extended Advertisement carries the full payload in ServiceData).
    if let Ok(parsed) = parse_receiving_payload(data, address) {
        return Ok(Some(parsed));
    }

    // 2. Regular BLE advertisement header (Version 2 Bloom-filter + slot hashes): read GATT slots on 0xFEF3.
    if is_fef3_gatt_slot_header(data) {
        return cached_gatt_slot(cache, address, data, read_fef3_gatt_slots(device, address)).await;
    }

    Ok(None)
}

async fn read_fef3_gatt_slots(
    device: &bluer::Device,
    address: Address,
) -> anyhow::Result<Option<([u8; 4], DeviceType, String, Address)>> {
    let was_connected = device.is_connected().await.unwrap_or(false);
    let mut connected_here = false;
    let attempt = async {
        if !was_connected {
            if let Err(error) = retry_bluetooth_connection(|| device.connect()).await {
                debug!("BLE FEF3 GATT connection failed for {address}: {error}");
                return Ok(None);
            }
            connected_here = true;
        }
        for _ in 0..25 {
            if device.is_services_resolved().await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let Ok(services) = device.services().await else {
            return Ok(None);
        };
        for service in services {
            if service.uuid().await.ok() != Some(FEF3_SERVICE_UUID) {
                continue;
            }
            if let Ok(characteristics) = service.characteristics().await {
                for ch in characteristics {
                    if let Ok(bytes) = ch.read().await {
                        if let Ok(parsed) = parse_receiving_payload(&bytes, address) {
                            return Ok::<_, anyhow::Error>(Some(parsed));
                        }
                    }
                }
            }
        }
        Ok(None)
    };
    let out = match tokio::time::timeout(Duration::from_secs(6), attempt).await {
        Ok(res) => res,
        Err(_) => {
            debug!("BLE FEF3 GATT slot read timed out for {address}");
            Ok(None)
        }
    };
    if connected_here {
        let _ = tokio::time::timeout(Duration::from_secs(2), device.disconnect()).await;
    }
    out
}

pub(crate) async fn discover(
    sender: broadcast::Sender<EndpointInfo>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    let mut known = HashMap::new();
    let resolved = Arc::new(GattSlotCache::default());
    let mut last_probe = HashMap::<Address, std::time::Instant>::new();
    let probing = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut pause_rx = outgoing_connections();
    'discovery: loop {
        while *pause_rx.borrow_and_update() > 0 {
            tokio::select! {
                _ = ctk.cancelled() => break 'discovery,
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        break 'discovery;
                    }
                }
            }
        }
        let Some(_scan) = DiscoveryScanGuard::start() else { continue };
        let events = adapter.discover_devices_with_changes().await?;
        pin_mut!(events);
        loop {
            let event = tokio::select! {
                biased;
                _ = ctk.cancelled() => break 'discovery,
                changed = pause_rx.changed() => {
                    match changed {
                        Ok(()) if *pause_rx.borrow() > 0 => {
                            debug!("Pausing Bluetooth discovery during outgoing connection");
                            break;
                        }
                        Ok(()) => continue,
                        Err(_) => break 'discovery,
                    }
                }
                event = events.next() => match event { Some(e) => e, None => break 'discovery },
            };
        match event {
            AdapterEvent::DeviceAdded(address) => {
                let adapter = adapter.clone();
                let sender = sender.clone();
                let probing = probing.clone();
                let Ok(device) = adapter.device(address) else {
                    continue;
                };
                let display = device.name().await.ok().flatten();
                let parsed = display.as_deref().and_then(|d| parse_name(d).ok());
                let nearby_service = device
                    .class()
                    .await
                    .ok()
                    .flatten()
                    .is_some()
                    && device
                        .uuids()
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|ids| ids.contains(&SERVICE_UUID));

                if parsed.is_some() || nearby_service {
                    let (endpoint, kind, name) = parsed
                        .map(|(id, kind, name)| {
                            (String::from_utf8_lossy(&id).into_owned(), kind, name)
                        })
                        .unwrap_or_else(|| {
                            (
                                String::new(),
                                DeviceType::Unknown,
                                display.unwrap_or_else(|| address.to_string()),
                            )
                        });
                    let id = format!("bluetooth://{address}");
                    let info = EndpointInfo {
                        fullname: endpoint,
                        id: id.clone(),
                        name: Some(name),
                        ip: Some(id),
                        port: None,
                        rtype: Some(kind),
                        present: Some(true),
                    };
                    if known
                        .get(&address)
                        .and_then(|p: &EndpointInfo| p.name.as_ref())
                        != info.name.as_ref()
                    {
                        info!(
                            "Bluetooth Nearby endpoint {} ({address})",
                            info.name.as_deref().unwrap_or("Unknown")
                        );
                        let _ = sender.send(info.clone());
                    }
                    known.insert(address, info);
                    continue;
                }

                // Check for BLE FEF3 Nearby advertisement
                let service_data = device.service_data().await.ok().flatten().unwrap_or_default();
                let Some(data) = service_data.get(&FEF3_SERVICE_UUID) else {
                    continue;
                };

                // An extended advertisement can supply both transports for the same endpoint.
                if let Ok((id_bytes, kind, name, classic_mac)) =
                    parse_receiving_payload(data, address)
                {
                    let endpoint = String::from_utf8_lossy(&id_bytes).into_owned();
                    let id = format!("bluetooth://{classic_mac}");
                    let address_type = device.address_type().await.ok();
                    let route =
                        select_ble_outbound_route(&id, address, address_type, advertised_l2cap_psm(data));
                    let info = EndpointInfo {
                        fullname: endpoint,
                        id,
                        name: Some(name),
                        ip: Some(route),
                        port: None,
                        rtype: Some(kind),
                        present: Some(true),
                    };
                    if known.get(&address).is_none_or(|previous| {
                        previous.name != info.name || previous.fullname != info.fullname || previous.ip != info.ip
                    }) {
                        info!(
                            "BLE FEF3 Nearby endpoint {} via {}",
                            info.name.as_deref().unwrap_or("Unknown"),
                            info.ip.as_deref().unwrap_or_default(),
                        );
                        let _ = sender.send(info.clone());
                    }
                    known.insert(address, info);
                    continue;
                }

                if !is_fef3_gatt_slot_header(data) {
                    continue;
                }

                // Do not reconnect/disconnect a resolved slot during an outgoing RFCOMM session.
                if resolved.lock().unwrap().get(&address).is_some_and(|(header, _)| header == data) {
                    continue;
                }
                if last_probe.get(&address).is_some_and(|started| started.elapsed() < Duration::from_secs(6)) {
                    continue;
                }
                last_probe.insert(address, std::time::Instant::now());
                let psm = advertised_l2cap_psm(data);
                let resolved = resolved.clone();
                // 2. Spawn GATT slot reader asynchronously so events.next() is NEVER blocked!
                {
                    let mut lock = probing.lock().unwrap();
                    if lock.contains(&address) {
                        continue;
                    }
                    lock.insert(address);
                }

                tokio::spawn(async move {
                    let res = resolve_ble_fef3_device(&device, address, &resolved).await;
                    probing.lock().unwrap().remove(&address);
                    if let Ok(Some((id_bytes, kind, name, classic_mac))) = res {
                        let endpoint = String::from_utf8_lossy(&id_bytes).into_owned();
                        let id = format!("bluetooth://{classic_mac}");
                        let info = EndpointInfo {
                            fullname: endpoint,
                            id: id.clone(),
                            name: Some(name),
                            ip: Some(select_ble_outbound_route(&id, address, device.address_type().await.ok(), psm)),
                            port: None,
                            rtype: Some(kind),
                            present: Some(true),
                        };
                        info!(
                            "BLE FEF3 Nearby endpoint {} via {}",
                            info.name.as_deref().unwrap_or("Unknown"),
                            info.ip.as_deref().unwrap_or_default(),
                        );
                        let _ = sender.send(info);
                    }
                });
            }
            AdapterEvent::DeviceRemoved(address) => {
                resolved.lock().unwrap().remove(&address);
                last_probe.remove(&address);
                if let Some(mut info) = known.remove(&address) {
                    if info.id == format!("bluetooth://{address}") {
                        info.present = Some(false);
                        let _ = sender.send(info);
                    }
                }
            }
            _ => {}
        }
        }
    }
    Ok(())
}

// Nearby regular advertisements publish one read-only GATT slot, including the RFCOMM MAC.
fn receiving_payload(endpoint: [u8; 4], info: &[u8], address: Address) -> anyhow::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    if info.len() > 131 {
        bail!("Endpoint info exceeds Nearby BLE limit");
    }
    let mut inner = vec![0x23];
    inner.extend_from_slice(&SERVICE_HASH);
    inner.extend_from_slice(&endpoint);
    inner.push(info.len() as u8);
    inner.extend_from_slice(info);
    inner.extend_from_slice(&address.0);
    inner.extend_from_slice(&[0, 0]); // No UWB address or WebRTC.
    let mut payload = vec![0x48]; // Medium version 2, socket version 2, regular advertisement.
    payload.extend_from_slice(&SERVICE_HASH);
    payload.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    payload.extend_from_slice(&inner);
    payload.extend_from_slice(&Sha256::digest(info)[..2]);
    Ok(payload)
}

async fn advertise_receiving(
    adapter: &bluer::Adapter,
    endpoint: [u8; 4],
    info: &[u8],
) -> anyhow::Result<(
    bluer::gatt::local::ApplicationHandle,
    bluer::adv::AdvertisementHandle,
    tokio_util::task::AbortOnDropHandle<Option<bluer::adv::AdvertisementHandle>>,
)> {
    use bluer::gatt::local::{Application, Characteristic, CharacteristicRead, ReqError, Service};
    use futures::FutureExt;
    use sha2::{Digest, Sha256};
    let service = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");
    let payload = receiving_payload(endpoint, info, adapter.address().await?)?;
    let ext_payload = payload.clone();
    let mut header = vec![0x41]; // Version 2, no extended advertisement, one GATT slot.
    // MurmurHash3_x64_128("NearbySharing", seed=0), five hashes, 80-bit bloom filter.
    header.extend_from_slice(&[0x20, 0, 0, 0x10, 2, 0, 0, 3, 0, 0]);
    header.extend_from_slice(&Sha256::digest(&payload)[..4]);
    header.extend_from_slice(&[0, 0]); // No BLE L2CAP listener; use advertised Classic MAC.
    let app = adapter
        .serve_gatt_application(Application {
            services: vec![Service {
                uuid: service,
                primary: true,
                characteristics: vec![Characteristic {
                    uuid: uuid!("00000000-0000-3000-8000-000000000000"),
                    read: Some(CharacteristicRead {
                        read: true,
                        fun: Box::new(move |request| {
                            let result = payload
                                .get(usize::from(request.offset)..)
                                .map(|bytes| bytes.to_vec())
                                .ok_or(ReqError::InvalidOffset);
                            debug!("Nearby receiving GATT slot read: offset={}", request.offset);
                            async move { result }.boxed()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })
        .await?;
    let advertisement = adapter
        .advertise(bluer::adv::Advertisement {
            service_data: [(service, header)].into(),
            ..Default::default()
        })
        .await?;
    // Optional advertising must not block RFCOMM acceptance or the UKEY2 handshake.
    let adapter = adapter.clone();
    let ext_advertisement = background_advertisement(async move {
        Ok(adapter.advertise(bluer::adv::Advertisement {
            advertisement_type: bluer::adv::Type::Broadcast,
            service_data: [(service, ext_payload)].into(),
            secondary_channel: Some(bluer::adv::SecondaryChannel::OneM),
            min_interval: Some(Duration::from_millis(100)),
            max_interval: Some(Duration::from_millis(250)),
            ..Default::default()
        }).await?)
    });
    info!("Nearby receiving BLE advertisement active (extended advertisement pending separately)");
    Ok((app, advertisement, ext_advertisement))
}

fn background_advertisement<T: Send + 'static>(
    registration: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
) -> tokio_util::task::AbortOnDropHandle<Option<T>> {
    tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        match registration.await {
            Ok(handle) => {
                info!("Optional extended receiving BLE advertisement active");
                Some(handle)
            }
            Err(error) => {
                warn!("Optional extended receiving BLE advertisement unavailable: {error}");
                None
            }
        }
    }))
}

async fn retry_bluetooth_connection<T, F, Fut>(mut attempt: F) -> bluer::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bluer::Result<T>>,
{
    for retry in 0..=8 {
        match attempt().await {
            Err(error)
                if retry < 8
                    && (error.kind == bluer::ErrorKind::InProgress
                        || (matches!(
                            error.kind,
                            bluer::ErrorKind::Failed
                                | bluer::ErrorKind::ConnectionAttemptFailed
                                | bluer::ErrorKind::Internal(bluer::InternalErrorKind::Io(_))
                        ) && (error.message.contains("br-connection-busy")
                            || error.message.contains("Resource busy")
                            || error.message.contains("Operation already in progress")))) =>
            {
                debug!("Nearby Bluetooth operation busy; retrying after controller settles ({retry}): {error}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            result => return result,
        }
    }
    unreachable!()
}

fn build_sdp_rfcomm_query(uuid: Uuid) -> [u8; 32] {
    let mut pdu = [0u8; 32];
    pdu[..8].copy_from_slice(&[0x06, 0x00, 0x01, 0x00, 0x1b, 0x35, 0x11, 0x1c]);
    pdu[8..24].copy_from_slice(uuid.as_bytes());
    pdu[24..].copy_from_slice(&[0x01, 0x00, 0x35, 0x03, 0x09, 0x00, 0x04, 0x00]);
    pdu
}

fn parse_sdp_rfcomm_channel(pdu: &[u8]) -> anyhow::Result<u8> {
    if pdu.len() < 7 || pdu[0] != 0x07 {
        bail!("Invalid SDP ServiceSearchAttributeResponse PDU");
    }
    for window in pdu[5..].windows(5) {
        if window[..4] == [0x19, 0x00, 0x03, 0x08] && (1..=30).contains(&window[4]) {
            return Ok(window[4]);
        }
    }
    bail!("No RFCOMM channel in SDP response")
}

async fn resolve_sdp_rfcomm_channel(
    target: Address,
    uuid: Uuid,
) -> std::io::Result<(u8, bluer::l2cap::SeqPacket)> {
    let seq = bluer::l2cap::SeqPacket::connect(bluer::l2cap::SocketAddr::new(
        target,
        AddressType::BrEdr,
        1,
    ))
    .await?;
    wait_bt_connected(seq.as_raw_fd()).await?;
    seq.send(&build_sdp_rfcomm_query(uuid)).await?;
    let mut buf = [0u8; 512];
    let n = seq.recv(&mut buf).await?;
    let channel = parse_sdp_rfcomm_channel(&buf[..n])
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::NotFound, err.to_string()))?;
    Ok((channel, seq))
}

async fn connect_unpaired_rfcomm(target: Address, channel: u8) -> std::io::Result<Stream> {
    let socket = bluer::rfcomm::Socket::new()?;
    // BT_SECURITY_LOW (1) uses HCI_AT_NO_BONDING (SSP Just-Works No-Bonding encryption),
    // which satisfies Android's listenUsingInsecureRfcommWithServiceRecord without OS pairing/bonding.
    let _ = socket.set_security(bluer::rfcomm::Security {
        level: bluer::rfcomm::SecurityLevel::Low,
        key_size: 0,
    });
    socket.bind(bluer::rfcomm::SocketAddr::any())?;
    let stream = socket
        .connect(bluer::rfcomm::SocketAddr::new(target, channel))
        .await?;
    wait_bt_connected(stream.as_raw_fd()).await?;
    Ok(stream)
}

async fn run_l2cap_receiver(
    adapter: bluer::Adapter,
    endpoint: [u8; 4],
    info: Vec<u8>,
    mut visibility: watch::Receiver<Visibility>,
    incoming: mpsc::Sender<(Transport, String)>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr};
    let mut receiving = None;
    loop {
        if *visibility.borrow_and_update() == Visibility::Invisible {
            receiving = None;
        } else if receiving.is_none() {
            let socket = Socket::new_stream()?;
            socket.set_security(Security { level: SecurityLevel::Low, key_size: 0 })?;
            socket.bind(SocketAddr::new(adapter.address().await?, AddressType::LePublic, 0))?;
            let psm = socket.local_addr()?.psm;
            let listener = socket.listen(4)?;
            let mut payload = receiving_payload(endpoint, &info, Address::any())?;
            // The extended advertisement carries the LE PSM directly, without a GATT slot or Classic address.
            payload.push(0x01);
            payload.extend_from_slice(&psm.to_be_bytes());
            let advertisement = adapter.advertise(bluer::adv::Advertisement {
                advertisement_type: bluer::adv::Type::Peripheral,
                service_data: [(FEF3_SERVICE_UUID, payload)].into(),
                secondary_channel: Some(bluer::adv::SecondaryChannel::OneM),
                ..Default::default()
            }).await?;
            info!("Nearby BLE L2CAP receive advertisement active on PSM {psm:#06x}");
            receiving = Some((listener, advertisement));
        }
        tokio::select! {
            _ = ctk.cancelled() => return Ok(()),
            changed = visibility.changed() => { changed?; }
            connection = async {
                match &receiving {
                    Some((listener, _)) => listener.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                let (mut stream, peer) = connection?;
                let validation = async {
                    let mut request = [0; 5];
                    stream.read_exact(&mut request).await?;
                    anyhow::ensure!(request == [0, 0, 0, 1, 3], "Unexpected Nearby L2CAP request");
                    stream.write_all(&[0, 0, 0, 1, 23]).await?;
                    Ok::<_, anyhow::Error>(())
                };
                match tokio::time::timeout(Duration::from_secs(3), validation).await {
                    Ok(Ok(())) if *visibility.borrow() != Visibility::Invisible => {
                        info!("Nearby BLE L2CAP accepted, starting authenticated Quick Share session");
                        let transport = l2cap_transport(stream, ctk.clone());
                        incoming.send((transport, format!("{L2CAP_SCHEME}{}", peer.addr))).await?;
                    }
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => debug!("Ignoring invalid Nearby BLE L2CAP connection: {error}"),
                    Err(_) => debug!("Nearby BLE L2CAP connection did not send a request"),
                }
            }
        }
    }
}

pub(crate) async fn connect_l2cap(address: &str, token: CancellationToken) -> anyhow::Result<Transport> {
    use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr};
    use prost::Message;

    let target = address.strip_prefix(L2CAP_SCHEME).context("Missing BLE L2CAP scheme")?;
    let (address, route) = target.split_once('/').context("Missing BLE L2CAP route")?;
    let (address_type, psm) = route.split_once('/').context("Missing BLE L2CAP PSM")?;
    let address: Address = address.parse()?;
    let address_type: AddressType = address_type.parse()?;
    let psm: u16 = psm.parse()?;
    anyhow::ensure!(address != Address::any() &&
        matches!(address_type, AddressType::LePublic | AddressType::LeRandom) &&
        (0x80..=0xff).contains(&psm), "Invalid BLE L2CAP endpoint");

    let stream = tokio::time::timeout(L2CAP_CONNECT_TIMEOUT, async {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        let _pause = OutgoingConnectGuard::pause_discovery(&adapter).await;
        let socket = Socket::new_stream()?;
        socket.set_security(Security { level: SecurityLevel::Low, key_size: 0 })?;
        socket.bind(SocketAddr::new(adapter.address().await?, AddressType::LePublic, 0))?;
        let mut stream = socket.connect(SocketAddr::new(address, address_type, psm)).await?;
        wait_bt_connected(stream.as_raw_fd()).await?;
        debug!("Nearby BLE L2CAP link connected, waiting for peer readiness");
        stream.write_all(&[0, 0, 0, 1, 3]).await?;
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).await?;
        anyhow::ensure!(ready == [0, 0, 0, 1, 23], "Unexpected Nearby L2CAP readiness response");

        let intro = BleControl {
            kind: 1,
            introduction: Some(BleIntroduction {
                service_hash: SERVICE_HASH.to_vec(),
                socket_version: 2,
                salt: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>()),
            }),
            acknowledgement: None,
        }.encode_to_vec();
        stream.write_u32(3 + intro.len() as u32).await?;
        stream.write_all(&[0; 3]).await?;
        stream.write_all(&intro).await?;
        Ok::<_, anyhow::Error>(stream)
    }).await.context("Nearby BLE L2CAP connection timed out")??;
    info!("Connected to Nearby BLE L2CAP on PSM {psm:#06x}");
    Ok(l2cap_transport(stream, token))
}

fn l2cap_transport(stream: bluer::l2cap::Stream, token: CancellationToken) -> Transport {
    let (transport, bridge) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(error) = relay_l2cap(stream, bridge, token).await {
            debug!("Nearby BLE L2CAP closed: {error:#}");
        }
    });
    Transport::new(transport)
}

async fn relay_l2cap(
    stream: bluer::l2cap::Stream,
    bridge: tokio::io::DuplexStream,
    token: CancellationToken,
) -> anyhow::Result<()> {
    use prost::Message;
    const MAX_FRAME: u32 = 5 * 1024 * 1024;
    let (mut ble_in, ble_out) = stream.into_split();
    let ble_out = tokio::sync::Mutex::new(ble_out);
    let (mut app_in, mut app_out) = tokio::io::split(bridge);
    let receive = async {
        loop {
            let outer_size = ble_in.read_u32().await?;
            anyhow::ensure!((4..=MAX_FRAME + 7).contains(&outer_size), "Invalid BLE frame size");
            let mut marker = [0; 3];
            ble_in.read_exact(&mut marker).await?;
            let size = match marker {
                // BLE socket control frames are separate from the Nearby Connections session.
                [0, 0, 0] => {
                    anyhow::ensure!(outer_size <= 256, "Oversized BLE control frame");
                    let mut control = [0; 253];
                    let body = &mut control[..(outer_size - 3) as usize];
                    ble_in.read_exact(body).await?;
                    match BleControl::decode(&*body)?.kind {
                        1 | 3 => continue, // The listener is dedicated to Nearby Sharing.
                        2 => return Ok(()),
                        _ => anyhow::bail!("Unsupported BLE control frame"),
                    }
                }
                SERVICE_HASH => {
                    anyhow::ensure!(outer_size > 7, "Missing Nearby BLE data");
                    let size = ble_in.read_u32().await?;
                    anyhow::ensure!(size == outer_size - 7, "Mismatched BLE data length");
                    size
                }
                _ => anyhow::bail!("Unexpected BLE service marker"),
            };
            anyhow::ensure!(size <= MAX_FRAME, "Nearby frame too large");
            let ack = BleControl {
                kind: 3,
                introduction: None,
                acknowledgement: Some(BleAcknowledgement {
                    service_hash: SERVICE_HASH.to_vec(),
                    size,
                }),
            }.encode_to_vec();
            let mut packet = Vec::with_capacity(7 + ack.len());
            packet.extend_from_slice(&((ack.len() + 3) as u32).to_be_bytes());
            packet.extend_from_slice(&[0; 3]);
            packet.extend_from_slice(&ack);
            ble_out.lock().await.write_all(&packet).await?;
            app_out.write_u32(size).await?;
            let copied = tokio::io::copy(&mut (&mut ble_in).take(size.into()), &mut app_out).await?;
            anyhow::ensure!(copied == u64::from(size), "Incomplete BLE frame");
        }
    };
    let send = async {
        loop {
            let size = app_in.read_u32().await?;
            anyhow::ensure!((1..=MAX_FRAME).contains(&size), "Invalid Nearby frame size");
            let mut header = [0; 11];
            header[..4].copy_from_slice(&(size + 7).to_be_bytes());
            header[4..7].copy_from_slice(&SERVICE_HASH);
            header[7..].copy_from_slice(&size.to_be_bytes());
            let mut ble_out = ble_out.lock().await;
            ble_out.write_all(&header).await?;
            let copied = tokio::io::copy(&mut (&mut app_in).take(size.into()), &mut *ble_out).await?;
            anyhow::ensure!(copied == u64::from(size), "Incomplete Nearby frame");
        }
    };
    tokio::select! {
        _ = token.cancelled() => Ok(()),
        result = receive => result,
        result = send => result,
    }
}

#[derive(prost::Message)]
struct BleControl {
    #[prost(uint32, tag = "1")]
    kind: u32,
    #[prost(message, optional, tag = "2")]
    introduction: Option<BleIntroduction>,
    #[prost(message, optional, tag = "4")]
    acknowledgement: Option<BleAcknowledgement>,
}

#[derive(prost::Message)]
struct BleIntroduction {
    #[prost(bytes = "vec", tag = "1")]
    service_hash: Vec<u8>,
    #[prost(uint32, tag = "2")]
    socket_version: u32,
    #[prost(string, tag = "3")]
    salt: String,
}

#[derive(prost::Message)]
struct BleAcknowledgement {
    #[prost(bytes = "vec", tag = "1")]
    service_hash: Vec<u8>,
    #[prost(uint32, tag = "2")]
    size: u32,
}

pub(crate) async fn run(
    endpoint: [u8; 4],
    encoded_info: String,
    mut visibility: watch::Receiver<Visibility>,
    mut connects: mpsc::Receiver<Connect>,
    incoming: mpsc::Sender<(Transport, String)>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    let old_alias = adapter.alias().await?;
    let old_visible = adapter.is_discoverable().await?;
    let old_timeout = adapter.discoverable_timeout().await?;
    let endpoint_info = URL_SAFE_NO_PAD.decode(encoded_info)?;
    let alias = device_name(endpoint, &endpoint_info)?;
    let le_only = std::env::var("RQS_BLE_L2CAP").as_deref() != Ok("0");
    let _le_receiver = le_only.then(|| {
        let adapter = adapter.clone();
        let info = endpoint_info.clone();
        let visibility = visibility.clone();
        let incoming = incoming.clone();
        let ctk = ctk.clone();
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = run_l2cap_receiver(adapter, endpoint, info, visibility, incoming, ctk).await {
                warn!("Nearby BLE L2CAP receive service failed: {error:#}");
            }
        }))
    });
    let mut profile = session
        .register_profile(Profile {
            uuid: SERVICE_UUID,
            name: Some("NearbySharing".into()),
            channel: Some(27), // Android caches SDP channels; keep Nearby stable across restarts.
            require_authentication: Some(false),
            require_authorization: Some(false),
            ..Default::default()
        })
        .await?;
    // Nearby authenticates with UKEY2 and transfer consent, rather than OS Bluetooth bonding.
    let result: anyhow::Result<()> = async {
        if !le_only {
            adapter.set_discoverable_timeout(0).await?;
        }
        let mut pending: HashMap<Address, oneshot::Sender<anyhow::Result<Transport>>> = HashMap::new();
        let mut channels = HashMap::<Address, RecentChannel>::new();
        let mut attempts = tokio::task::JoinSet::new();
        let mut receiving = None;
        loop {
            let visible = *visibility.borrow_and_update() != Visibility::Invisible;
            if !le_only {
                adapter.set_alias(if visible { alias.clone() } else { old_alias.clone() }).await?;
                adapter.set_discoverable(visible).await?;
            }
            if visible && !le_only && receiving.is_none() {
                match advertise_receiving(&adapter, endpoint, &endpoint_info).await {
                    Ok(handles) => receiving = Some(handles),
                    Err(error) => warn!("Nearby receiving BLE/GATT advertisement failed: {error}"),
                }
            } else if !visible { receiving = None; }
            tokio::select! {
                _ = ctk.cancelled() => break,
                changed = visibility.changed() => { if changed.is_err() { break; } },
                Some(request) = connects.recv() => {
                    pending.retain(|_, reply| !reply.is_closed());
                    if pending.contains_key(&request.address) {
                        let _ = request.reply.send(Err(anyhow!("Bluetooth connection already pending")));
                        continue;
                    }
                    let target = request.address;
                    let peer_endpoint = request.endpoint;
                    channels.retain(|_, entry| entry.verified.elapsed() < RFCOMM_REUSE_AGE);
                    let cached = channels.remove(&target).filter(|entry| Some(entry.endpoint) == peer_endpoint);
                    pending.insert(request.address, request.reply);
                    let adapter_for_connect = adapter.clone();
                    attempts.spawn(async move {
                        let _pause_discovery = OutgoingConnectGuard::pause_discovery(&adapter_for_connect).await;
                        if let Some(entry) = cached {
                            match tokio::time::timeout(
                                RFCOMM_REUSE_TIMEOUT,
                                connect_unpaired_rfcomm(target, entry.channel),
                            ).await {
                                Ok(Ok(stream)) => {
                                    info!("Reused recent Nearby RFCOMM channel for the same endpoint");
                                    return (target, Ok(Transport::new(stream)), Some(entry));
                                }
                                Ok(Err(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                                    return (target, Err(error.into()), None);
                                }
                                _ => debug!("Recent Nearby RFCOMM channel was unavailable; querying SDP"),
                            }
                        }
                        // Connect via kernel L2CAP SDP (PSM 1) + unpaired RFCOMM socket (BT_SECURITY_LOW / HCI_AT_NO_BONDING)
                        // so BlueZ D-Bus never triggers OS Bluetooth pairing or persistent bonding.
                        let result = async {
                            info!("Resolving Nearby RFCOMM service {SERVICE_UUID} on {target} via direct L2CAP SDP");
                            let stream = tokio::time::timeout(SERVICE_CONNECT_TIMEOUT, async {
                                // Keep the SDP L2CAP socket open while RFCOMM connects over the same BR/EDR ACL link.
                                let (channel, resolved, _sdp_keepalive) = match retry_bluetooth_connection(|| async {
                                    resolve_sdp_rfcomm_channel(target, SERVICE_UUID)
                                        .await
                                        .map_err(bluer::Error::from)
                                })
                                .await
                                {
                                    Ok((channel, seq)) => {
                                        info!("Resolved Nearby RFCOMM channel {channel} on {target}");
                                        (channel, true, Some(seq))
                                    }
                                    // bluer wraps the OS error text as "IO error ...: Host is down (os error 112)".
                                    Err(err)
                                        if err.message.contains("os error 112")
                                            || err.message.contains("Host is down") =>
                                    {
                                        return Err(err);
                                    }
                                    Err(err) => {
                                        debug!("SDP channel query on {target} failed ({err}); falling back to RFCOMM channel 27");
                                        (27, false, None)
                                    }
                                };
                                let stream = retry_bluetooth_connection(|| async {
                                    connect_unpaired_rfcomm(target, channel)
                                        .await
                                        .map_err(bluer::Error::from)
                                })
                                .await?;
                                let verified = peer_endpoint.filter(|_| resolved).map(|endpoint| RecentChannel {
                                    endpoint,
                                    channel,
                                    verified: tokio::time::Instant::now(),
                                });
                                Ok::<_, bluer::Error>((Transport::new(stream), verified))
                            })
                            .await
                            .context("Nearby service connection timed out")?
                            .with_context(|| format!("Nearby service on {target} is not accepting connections"))?;
                            Ok::<_, anyhow::Error>(stream)
                        }
                        .await;
                        match result {
                            Ok((transport, verified)) => (target, Ok(transport), verified),
                            Err(error) => (target, Err(error), None),
                        }
                    });
                }
                Some(done) = attempts.join_next() => {
                    if let Ok((address, res, verified)) = done {
                        if let Some(reply) = pending.remove(&address) {
                            if reply.send(res).is_ok() {
                                if let Some(entry) = verified {
                                    channels.insert(address, entry);
                                }
                            }
                        }
                    }
                }
                Some(request) = profile.next() => {
                    let address = request.device();
                    if let Some(reply) = pending.remove(&address) {
                        let _ = reply.send(request.accept().map(Transport::new).map_err(Into::into));
                    } else if visible && !le_only {
                        let stream = request.accept()?;
                        incoming.send((Transport::new(stream), format!("bluetooth://{address}"))).await?;
                    } else { request.reject(bluer::rfcomm::ReqError::Rejected); }
                }
            }
        }
        Ok(())
    }.await;
    // Do not overwrite a name changed by another application while we were running.
    if !le_only && adapter.alias().await.ok().as_deref() == Some(&alias) {
        if let Err(err) = adapter.set_alias(old_alias).await {
            warn!("Restoring Bluetooth name: {err}");
        }
    }
    if !le_only {
        if let Err(err) = adapter.set_discoverable(old_visible).await {
            warn!("Restoring Bluetooth visibility: {err}");
        }
        if let Err(err) = adapter.set_discoverable_timeout(old_timeout).await {
            warn!("Restoring Bluetooth timeout: {err}");
        }
    }
    result
}

pub(crate) async fn connect(
    sender: &mpsc::Sender<Connect>,
    address: &str,
    endpoint: Option<&str>,
) -> anyhow::Result<Transport> {
    let address = address
        .strip_prefix("bluetooth://")
        .context("Invalid Bluetooth address")?
        .parse()?;
    let (reply, receive) = oneshot::channel();
    let endpoint = endpoint
        .filter(|id| id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .and_then(|id| id.as_bytes().try_into().ok());
    sender.send(Connect { address, endpoint, reply }).await?;
    tokio::time::timeout(CONNECTION_TIMEOUT, receive)
        .await
        .context("Bluetooth connection timed out")??
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::gen_mdns_endpoint_info;

    #[tokio::test]
    async fn unchanged_gatt_slots_do_not_reconnect_and_changed_slots_are_read() {
        let cache = GattSlotCache::default();
        let address = Address([2, 0, 0, 0, 0, 1]);
        let endpoint = (*b"TEST", DeviceType::Phone, "Test phone".to_owned(), address);
        let first = cached_gatt_slot(&cache, address, b"slot-a", async { Ok(Some(endpoint)) }).await.unwrap();
        assert!(first.is_some());
        let cached = cached_gatt_slot(&cache, address, b"slot-a", async { panic!("Unchanged slot must not reconnect") }).await.unwrap();
        assert_eq!(cached.unwrap().2, "Test phone");
        let updated = (*b"NEW!", DeviceType::Phone, "Updated phone".to_owned(), address);
        let changed = cached_gatt_slot(&cache, address, b"slot-b", async { Ok(Some(updated)) }).await.unwrap();
        assert_eq!(changed.unwrap().2, "Updated phone");
        assert!(cached_gatt_slot(&cache, address, b"slot-c", async { Ok(None) }).await.unwrap().is_none());
        assert!(cached_gatt_slot(&cache, address, b"slot-c", async { Err(anyhow!("probe failed")) }).await.is_err());
    }

    #[tokio::test]
    async fn profile_connection_retries_busy_but_not_authentication_failures() {
        let mut calls = 0;
        retry_bluetooth_connection(|| {
            calls += 1;
            std::future::ready(if calls <= 2 {
                Err(bluer::Error { kind: bluer::ErrorKind::Failed, message: "br-connection-busy".into() })
            } else { Ok(()) })
        }).await.unwrap();
        assert_eq!(calls, 3);
        calls = 0;
        let result = retry_bluetooth_connection(|| {
            calls += 1;
            std::future::ready(Err::<(), _>(bluer::Error { kind: bluer::ErrorKind::AuthenticationRejected, message: "Rejected".into() }))
        }).await;
        assert_eq!(result.unwrap_err().kind, bluer::ErrorKind::AuthenticationRejected);
        assert_eq!(calls, 1);
        calls = 0;
        assert!(retry_bluetooth_connection(|| {
            calls += 1;
            std::future::ready(Err::<(), _>(bluer::Error { kind: bluer::ErrorKind::InProgress, message: "busy".into() }))
        }).await.is_err());
        assert_eq!(calls, 9, "Busy retry must remain bounded");
    }

    #[tokio::test]
    async fn stalled_optional_advertisement_does_not_block_and_is_cancelled_on_drop() {
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        struct Registration(Option<oneshot::Sender<()>>);
        impl Drop for Registration {
            fn drop(&mut self) { let _ = self.0.take().unwrap().send(()); }
        }
        let task = background_advertisement(async move {
            let _registration = Registration(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<anyhow::Result<()>>().await
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx).await.unwrap().unwrap();
        assert!(!task.is_finished());
        // The caller remains available to accept connections while registration hangs.
        drop(task);
        tokio::time::timeout(Duration::from_secs(1), dropped_rx).await.unwrap().unwrap();

        let (released_tx, released_rx) = oneshot::channel();
        let task = background_advertisement(async { Ok(Registration(Some(released_tx))) });
        while !task.is_finished() { tokio::task::yield_now().await; }
        drop(task);
        tokio::time::timeout(Duration::from_secs(1), released_rx).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_preserves_service_errors_and_targets_the_selected_device() {
        assert!(CONNECTION_TIMEOUT > SERVICE_CONNECT_TIMEOUT + RFCOMM_REUSE_TIMEOUT);
        let (sender, mut receiver) = mpsc::channel::<Connect>(1);
        let connect = connect(&sender, "bluetooth://02:00:00:00:00:01", Some("Ab01"));
        let service = async {
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.address.to_string(), "02:00:00:00:00:01");
            assert_eq!(request.endpoint, Some(*b"Ab01"));
            let _ = request.reply.send(Err(anyhow!("org.bluez.Error.NotAvailable")));
        };
        let (result, ()) = tokio::join!(connect, service);
        assert!(result.err().unwrap().to_string().contains("NotAvailable"));
        assert!(super::connect(&sender, "bad address", None).await.is_err());
    }
    #[test]
    fn receiving_slot_contains_endpoint_info_and_classic_address() {
        let info = vec![7; 27];
        let mac = Address([0xe8, 0xc8, 0x29, 0x0b, 0xad, 0x23]);
        let bytes = receiving_payload(*b"TEST", &info, mac).unwrap();
        assert_eq!(&bytes[..4], &[0x48, 0xfc, 0x9f, 0x5e]);
        let length = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(length, 9 + info.len() + 8);
        assert_eq!(
            &bytes[8..17],
            &[0x23, 0xfc, 0x9f, 0x5e, b'T', b'E', b'S', b'T', 27]
        );
        assert_eq!(&bytes[17..44], &info);
        assert_eq!(&bytes[44..50], &mac.0);
        assert_eq!(bytes.len(), 8 + length + 2);
        assert!(receiving_payload(*b"TEST", &[0; 132], mac).is_err());
    }

    #[test]
    fn nearby_name_round_trip_and_untrusted_input() {
        let info = URL_SAFE_NO_PAD
            .decode(gen_mdns_endpoint_info(
                DeviceType::Laptop as u8,
                "Packet Lab",
            ))
            .unwrap();
        let name = device_name(*b"TEST", &info).unwrap();
        let (id, kind, display) = parse_name(&name).unwrap();
        assert_eq!(id, *b"TEST");
        assert_eq!(kind, DeviceType::Laptop);
        assert_eq!(display, "Packet Lab");
        for data in [vec![], vec![0; 16], vec![0x23; 20]] {
            assert!(parse_name(&URL_SAFE_NO_PAD.encode(data)).is_err());
        }
        let mut raw = URL_SAFE_NO_PAD.decode(&name).unwrap();
        raw[5] ^= 1;
        assert!(parse_name(&URL_SAFE_NO_PAD.encode(raw)).is_err());
        assert!(device_name(*b"TEST", &[0; 132]).is_err());
    }

    #[test]
    fn ble_fef3_slot_and_extended_payload_round_trip() {
        let info = URL_SAFE_NO_PAD
            .decode(gen_mdns_endpoint_info(
                DeviceType::Phone as u8,
                "Friend's Pixel 9",
            ))
            .unwrap();
        let classic_mac = Address([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        let ble_random_mac = Address([0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54]);

        // 1. Wrapped GATT slot payload
        let slot_bytes = receiving_payload(*b"PX9A", &info, classic_mac).unwrap();
        assert_eq!(advertised_l2cap_psm(&slot_bytes), None);
        let mut extended = slot_bytes.clone();
        // The Samsung capture included both a PSM and instant-connection fields.
        extended.extend_from_slice(&[3, 0, 175, 4, 0, 1, 2, 3]);
        assert_eq!(advertised_l2cap_psm(&extended), Some(175));
        assert_eq!(advertised_l2cap_psm(&extended[..slot_bytes.len() + 2]), None);
        let invalid_psm = slot_bytes.len() + 2;
        extended[invalid_psm] = 7;
        assert_eq!(advertised_l2cap_psm(&extended), None);
        let mut valid_extended = slot_bytes.clone();
        valid_extended.extend_from_slice(&[3, 0, 129, 4, 0, 1, 2, 3]);
        assert_eq!(
            select_ble_outbound_route(
                &format!("bluetooth://{classic_mac}"),
                ble_random_mac,
                Some(AddressType::LeRandom),
                advertised_l2cap_psm(&valid_extended),
            ),
            format!("{L2CAP_SCHEME}{ble_random_mac}/random/129"),
        );
        assert_eq!(
            select_ble_outbound_route(
                &format!("bluetooth://{ble_random_mac}"),
                ble_random_mac,
                Some(AddressType::LePublic),
                advertised_l2cap_psm(&valid_extended),
            ),
            format!("{L2CAP_SCHEME}{ble_random_mac}/public/129"),
        );
        let mut header = [0; 17];
        header[0] = 0x51;
        header[15..].copy_from_slice(&129u16.to_be_bytes());
        assert_eq!(advertised_l2cap_psm(&header), Some(129));
        assert_eq!(advertised_l2cap_psm(&header[..15]), None);
        assert_eq!(advertised_l2cap_psm(&header[..16]), None);
        let (id, kind, name, resolved_mac) =
            parse_receiving_payload(&slot_bytes, ble_random_mac).unwrap();
        assert_eq!(id, *b"PX9A");
        assert_eq!(kind, DeviceType::Phone);
        assert_eq!(name, "Friend's Pixel 9");
        assert_eq!(resolved_mac, classic_mac);

        // 2. Direct BLE extended advertisement inner payload
        let inner_len = u32::from_be_bytes(slot_bytes[4..8].try_into().unwrap()) as usize;
        let inner_bytes = &slot_bytes[8..8 + inner_len];
        let (id2, kind2, name2, resolved_mac2) =
            parse_receiving_payload(inner_bytes, ble_random_mac).unwrap();
        assert_eq!(id2, *b"PX9A");
        assert_eq!(kind2, DeviceType::Phone);
        assert_eq!(name2, "Friend's Pixel 9");
        assert_eq!(resolved_mac2, classic_mac);

        // 3. Fallback to BLE address if Classic MAC is all-zero
        let zero_mac_bytes = receiving_payload(*b"PX9B", &info, Address([0; 6])).unwrap();
        let (_, _, _, fallback_resolved) =
            parse_receiving_payload(&zero_mac_bytes, ble_random_mac).unwrap();
        assert_eq!(fallback_resolved, ble_random_mac);

        // 4. Rejects malformed payloads
        for invalid in [vec![], vec![0x48, 0xfc, 0x9f, 0x5e, 0, 0, 1, 0], vec![0x23; 10]] {
            assert!(parse_receiving_payload(&invalid, ble_random_mac).is_err());
        }
    }

    #[test]
    fn sdp_rfcomm_query_and_response_parser_extract_dynamic_channel() {
        let query = build_sdp_rfcomm_query(SERVICE_UUID);
        assert_eq!(query[0], 0x06);
        assert_eq!(&query[3..5], &[0x00, 0x1b]);
        assert_eq!(&query[8..24], SERVICE_UUID.as_bytes());

        // Live SDP ServiceSearchAttributeResponse captured from Galaxy S24 Ultra (RFCOMM channel 11).
        let response = [
            0x07, 0x00, 0x01, 0x00, 0x19, 0x00, 0x16, 0x35, 0x14, 0x36, 0x00, 0x11, 0x09, 0x00,
            0x04, 0x35, 0x0c, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00, 0x03, 0x08,
            0x0b, 0x00,
        ];
        assert_eq!(parse_sdp_rfcomm_channel(&response).unwrap(), 11);
        assert!(parse_sdp_rfcomm_channel(&[0x07, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00]).is_err());
        assert!(parse_sdp_rfcomm_channel(&[0x01, 0x00, 0x01]).is_err());
    }
}
