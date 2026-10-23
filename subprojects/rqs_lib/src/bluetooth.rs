use crate::{
    DeviceType,
    hdl::{EndpointInfo, Visibility},
    transport::Transport,
    utils::{gen_mdns_endpoint_info, parse_mdns_endpoint_info},
};
use anyhow::{Context, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bluer::{
    AdapterEvent, Address,
    rfcomm::{Profile, Stream},
};
use futures::{StreamExt, pin_mut};
use std::{collections::HashMap, time::Duration};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

// Nearby's Java nameUUIDFromBytes("NearbySharing"), without a namespace prefix.
const SERVICE_UUID: Uuid = uuid!("a82efa21-ae5c-3dde-9bbc-f16da7b16c5a");
const SERVICE_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];

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

pub(crate) async fn discover(
    sender: broadcast::Sender<EndpointInfo>,
    ctk: CancellationToken,
) -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    let events = adapter.discover_devices_with_changes().await?;
    pin_mut!(events);
    let mut known = HashMap::new();
    loop {
        let event = tokio::select! {
            _ = ctk.cancelled() => break,
            event = events.next() => match event { Some(e) => e, None => break },
        };
        match event {
            AdapterEvent::DeviceAdded(address) => {
                let device = adapter.device(address)?;
                if let Some(display) = device.name().await? {
                    let parsed = parse_name(&display).ok();
                    // Some Android builds retain their normal Bluetooth name but expose Nearby's SDP UUID.
                    let nearby_service = device.class().await?.is_some()
                        && device
                            .uuids()
                            .await?
                            .is_some_and(|ids| ids.contains(&SERVICE_UUID));
                    if parsed.is_some() || nearby_service {
                        let (endpoint, kind, name) = parsed
                            .map(|(id, kind, name)| {
                                (String::from_utf8_lossy(&id).into_owned(), kind, name)
                            })
                            .unwrap_or_else(|| (String::new(), DeviceType::Unknown, display));
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
                    }
                }
            }
            AdapterEvent::DeviceRemoved(address) => {
                if let Some(mut info) = known.remove(&address) {
                    info.present = Some(false);
                    let _ = sender.send(info);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) async fn run(
    endpoint: [u8; 4],
    name: String,
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
    let encoded_info = gen_mdns_endpoint_info(DeviceType::Laptop as u8, &name);
    let alias = device_name(endpoint, &URL_SAFE_NO_PAD.decode(encoded_info)?)?;
    let mut profile = session
        .register_profile(Profile {
            uuid: SERVICE_UUID,
            name: Some("NearbySharing".into()),
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
        loop {
            let visible = *visibility.borrow_and_update() != Visibility::Invisible;
            adapter.set_alias(if visible { alias.clone() } else { old_alias.clone() }).await?;
            adapter.set_discoverable(visible).await?;
            tokio::select! {
                _ = ctk.cancelled() => break,
                changed = visibility.changed() => { if changed.is_err() { break; } },
                Some(request) = connects.recv() => {
                    pending.retain(|_, reply| !reply.is_closed());
                    if pending.contains_key(&request.address) {
                        let _ = request.reply.send(Err(anyhow!("Bluetooth connection already pending")));
                        continue;
                    }
                    let device = adapter.device(request.address)?;
                    pending.insert(request.address, request.reply);
                    attempts.spawn(async move { (request.address, device.connect_profile(&SERVICE_UUID).await) });
                }
                Some(done) = attempts.join_next() => {
                    if let Ok((address, Err(err))) = done {
                        if let Some(reply) = pending.remove(&address) { let _ = reply.send(Err(err.into())); }
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
    tokio::time::timeout(Duration::from_secs(15), receive)
        .await
        .context("Bluetooth connection timed out")??
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
