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
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

// Nearby's Java nameUUIDFromBytes("NearbySharing"), without a namespace prefix.
const SERVICE_UUID: Uuid = uuid!("a82efa21-ae5c-3dde-9bbc-f16da7b16c5a");
const FEF3_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");
const SERVICE_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Connect {
    pub address: Address,
    pub reply: oneshot::Sender<anyhow::Result<Transport>>,
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
    let classic_address = if inner.len() >= info_end + 6 && inner[info_end..info_end + 6] != [0; 6]
    {
        Address(inner[info_end..info_end + 6].try_into()?)
    } else {
        fallback_address
    };
    Ok((endpoint, kind, name, classic_address))
}

fn is_fef3_gatt_slot_header(data: &[u8]) -> bool {
    // Nearby v2 BLE GATT slot header:
    // byte 0: version 2 (0b0100_0000), bit 3 = 0 (regular GATT slot header, not 0x4a fast adv),
    // bits 0..=2 = slot count (1..=7).
    // Followed by 10-byte bloom filter + 4 bytes per slot hash + 2-byte PSM/reserved = 13 + 4 * slots.
    if data.len() < 17 || (data[0] & 0xe8) != 0x40 {
        return false;
    }
    let slots = usize::from(data[0] & 0x07);
    slots >= 1 && data.len() == 13 + 4 * slots
}

async fn resolve_ble_fef3_device(
    device: &bluer::Device,
    address: Address,
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
        return read_fef3_gatt_slots(device, address).await;
    }

    Ok(None)
}

async fn read_fef3_gatt_slots(
    device: &bluer::Device,
    address: Address,
) -> anyhow::Result<Option<([u8; 4], DeviceType, String, Address)>> {
    let was_connected = device.is_connected().await.unwrap_or(false);
    let attempt = async {
        if !was_connected {
            let _ = device.connect().await;
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
    if !was_connected {
        let _ = device.disconnect().await;
    }
    out
}

pub(crate) async fn discover(
    sender: broadcast::Sender<EndpointInfo>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    let events = adapter.discover_devices_with_changes().await?;
    pin_mut!(events);
    let mut known = HashMap::new();
    let probing = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    loop {
        let event = tokio::select! {
            _ = ctk.cancelled() => break,
            event = events.next() => match event { Some(e) => e, None => break },
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

                // 1. Immediate decode if service_data already contains the full payload (BLE 5.0 extended adv)
                if let Ok((id_bytes, kind, name, classic_mac)) = parse_receiving_payload(data, address) {
                    let endpoint = String::from_utf8_lossy(&id_bytes).into_owned();
                    let id = format!("bluetooth://{classic_mac}");
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
                            "BLE FEF3 Nearby endpoint {} (BLE {address} -> Classic {classic_mac})",
                            info.name.as_deref().unwrap_or("Unknown")
                        );
                        let _ = sender.send(info.clone());
                    }
                    known.insert(address, info);
                    continue;
                }

                if !is_fef3_gatt_slot_header(data) {
                    continue;
                }

                // 2. Spawn GATT slot reader asynchronously so events.next() is NEVER blocked!
                {
                    let mut lock = probing.lock().unwrap();
                    if lock.contains(&address) {
                        continue;
                    }
                    lock.insert(address);
                }

                tokio::spawn(async move {
                    let res = resolve_ble_fef3_device(&device, address).await;
                    probing.lock().unwrap().remove(&address);
                    if let Ok(Some((id_bytes, kind, name, classic_mac))) = res {
                        let endpoint = String::from_utf8_lossy(&id_bytes).into_owned();
                        let id = format!("bluetooth://{classic_mac}");
                        let info = EndpointInfo {
                            fullname: endpoint,
                            id: id.clone(),
                            name: Some(name),
                            ip: Some(id),
                            port: None,
                            rtype: Some(kind),
                            present: Some(true),
                        };
                        info!(
                            "BLE FEF3 Nearby endpoint {} (BLE {address} -> Classic {classic_mac})",
                            info.name.as_deref().unwrap_or("Unknown")
                        );
                        let _ = sender.send(info);
                    }
                });
            }
            AdapterEvent::DeviceRemoved(address) => {
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
    Option<bluer::adv::AdvertisementHandle>,
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
    let ext_advertisement = adapter
        .advertise(bluer::adv::Advertisement {
            advertisement_type: bluer::adv::Type::Broadcast,
            service_data: [(service, ext_payload)].into(),
            secondary_channel: Some(bluer::adv::SecondaryChannel::OneM),
            min_interval: Some(Duration::from_millis(100)),
            max_interval: Some(Duration::from_millis(250)),
            ..Default::default()
        })
        .await
        .ok();
    info!(
        "Nearby receiving BLE advertisement active (extended_adv={})",
        ext_advertisement.is_some()
    );
    Ok((app, advertisement, ext_advertisement))
}

pub(crate) async fn run(
    endpoint: [u8; 4],
    encoded_info: String,
    mut visibility: watch::Receiver<Visibility>,
    mut connects: mpsc::Receiver<Connect>,
    incoming: mpsc::Sender<(Stream, String)>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    let old_alias = adapter.alias().await?;
    let old_visible = adapter.is_discoverable().await?;
    let old_timeout = adapter.discoverable_timeout().await?;
    let endpoint_info = URL_SAFE_NO_PAD.decode(encoded_info)?;
    let alias = device_name(endpoint, &endpoint_info)?;
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
        adapter.set_discoverable_timeout(0).await?;
        let mut pending: HashMap<Address, oneshot::Sender<anyhow::Result<Transport>>> = HashMap::new();
        let mut attempts = tokio::task::JoinSet::new();
        let mut receiving = None;
        loop {
            let visible = *visibility.borrow_and_update() != Visibility::Invisible;
            adapter.set_alias(if visible { alias.clone() } else { old_alias.clone() }).await?;
            adapter.set_discoverable(visible).await?;
            if visible && receiving.is_none() {
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
                    pending.insert(request.address, request.reply);
                    let adapter_clone = adapter.clone();
                    attempts.spawn(async move {
                        // BlueZ resolves the advertised service's channel via SDP; Android channels vary.
                        let result = async {
                            info!("Resolving Nearby RFCOMM service {SERVICE_UUID} on {target}");
                            tokio::time::timeout(SERVICE_CONNECT_TIMEOUT, async {
                                // A BLE advertisement can name a Classic address BlueZ has never discovered.
                                let device = if adapter_clone.device_addresses().await?.contains(&target) {
                                    adapter_clone.device(target)?
                                } else {
                                    info!("Creating Classic Bluetooth device object for advertised address {target}");
                                    let device = adapter_clone.connect_device(target, AddressType::BrEdr).await?;
                                    while !device.is_services_resolved().await? {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                    device
                                };
                                device.connect_profile(&SERVICE_UUID).await
                            })
                                .await.context("Nearby service connection timed out")?
                                .with_context(|| format!("Nearby service on {target} is not accepting connections"))?;
                            Ok::<_, anyhow::Error>(())
                        }.await;
                        (target, result)
                    });
                }
                Some(done) = attempts.join_next() => {
                    if let Ok((address, res)) = done {
                        match res {
                            Ok(()) => {
                                // Waiting for profile.next() callback
                            }
                            Err(err) => {
                                if let Some(reply) = pending.remove(&address) {
                                    let _ = reply.send(Err(err));
                                }
                            }
                        }
                    }
                }
                Some(request) = profile.next() => {
                    let address = request.device();
                    if let Some(reply) = pending.remove(&address) {
                        let _ = reply.send(request.accept().map(Transport::new).map_err(Into::into));
                    } else if visible {
                        let stream = request.accept()?;
                        incoming.send((stream, format!("bluetooth://{address}"))).await?;
                    } else { request.reject(bluer::rfcomm::ReqError::Rejected); }
                }
            }
        }
        Ok(())
    }.await;
    // Do not overwrite a name changed by another application while we were running.
    if adapter.alias().await.ok().as_deref() == Some(&alias) {
        if let Err(err) = adapter.set_alias(old_alias).await {
            warn!("Restoring Bluetooth name: {err}");
        }
    }
    if let Err(err) = adapter.set_discoverable(old_visible).await {
        warn!("Restoring Bluetooth visibility: {err}");
    }
    if let Err(err) = adapter.set_discoverable_timeout(old_timeout).await {
        warn!("Restoring Bluetooth timeout: {err}");
    }
    result
}

pub(crate) async fn connect(
    sender: &mpsc::Sender<Connect>,
    address: &str,
) -> anyhow::Result<Transport> {
    let address = address
        .strip_prefix("bluetooth://")
        .context("Invalid Bluetooth address")?
        .parse()?;
    let (reply, receive) = oneshot::channel();
    sender.send(Connect { address, reply }).await?;
    tokio::time::timeout(CONNECTION_TIMEOUT, receive)
        .await
        .context("Bluetooth connection timed out")??
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::gen_mdns_endpoint_info;

    #[tokio::test]
    async fn connection_preserves_service_errors_and_targets_the_selected_device() {
        assert!(CONNECTION_TIMEOUT > SERVICE_CONNECT_TIMEOUT);
        let (sender, mut receiver) = mpsc::channel::<Connect>(1);
        let connect = connect(&sender, "bluetooth://02:00:00:00:00:01");
        let service = async {
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.address.to_string(), "02:00:00:00:00:01");
            let _ = request.reply.send(Err(anyhow!("org.bluez.Error.NotAvailable")));
        };
        let (result, ()) = tokio::join!(connect, service);
        assert!(result.err().unwrap().to_string().contains("NotAvailable"));
        assert!(super::connect(&sender, "bad address").await.is_err());
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
}
