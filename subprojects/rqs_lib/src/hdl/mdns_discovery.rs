use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::SocketAddrV4;

use crate::connection::Connector;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::DeviceType;
use crate::utils::{is_not_self_ip, parse_mdns_endpoint_info};

#[derive(Debug, Clone, Default)]
pub struct EndpointInfo {
    pub fullname: String,
    pub id: String,
    pub name: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    pub rtype: Option<DeviceType>,
    pub present: Option<bool>,
}

pub struct MDnsDiscovery {
    connector: Connector,
    daemon: ServiceDaemon,
    sender: broadcast::Sender<EndpointInfo>,
}

impl MDnsDiscovery {
    pub fn new(sender: broadcast::Sender<EndpointInfo>) -> Result<Self, anyhow::Error> {
        let connector = Connector::from_env()?;
        let daemon = ServiceDaemon::new()?;

        Ok(Self {
            connector,
            daemon,
            sender,
        })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("MDnsDiscovery: service starting");

        let service_type = "_FC9F5ED42C8A._tcp.local.";
        let receiver = self.daemon.browse(service_type)?;

        // Map with fullname as key and EndpointInfo as value
        let mut cache: HashMap<String, EndpointInfo> = HashMap::new();

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("MDnsDiscovery: tracker cancelled, breaking");
                    break;
                }
                r = receiver.recv_async() => {
                    match r {
                        Ok(event) => {
                            match event {
                                ServiceEvent::ServiceResolved(info) => {
                                    let port = info.get_port();

                                    // Decode the "n" text properties
                                    let n = match info.get_property("n") {
                                        Some(_n) => _n,
                                        None => continue,
                                    };

                                    // Parse the endpoint info
                                    let (dt, dn) = match parse_mdns_endpoint_info(n.val_str()) {
                                        Ok(r) => r,
                                        Err(_) => continue
                                    };

                                    let mut candidates: Vec<SocketAddrV4> = info.get_addresses_v4()
                                        .into_iter().filter(|ip| is_not_self_ip(ip))
                                        .map(|ip| SocketAddrV4::new(*ip, port)).collect();
                                    candidates.sort_unstable();
                                    if let Some(peer) = self.connector.txt_candidate(
                                        info.get_property("IPv4").map(|p| p.val_str()), port,
                                    ).filter(|peer| is_not_self_ip(peer.ip())) {
                                        candidates.retain(|addr| *addr != peer);
                                        candidates.insert(0, peer);
                                    }
                                    let fullname = info.get_fullname().to_string();
                                    for candidate in candidates {
                                        let ip = candidate.ip();
                                        let ip_port = candidate.to_string();
                                        let result = tokio::select! {
                                            _ = ctk.cancelled() => return Ok(()),
                                            result = self.connector.connect(&ip_port) => result,
                                        };
                                        if let Err(err) = result {
                                            debug!("ServiceResolved: {ip_port}: {err}");
                                            continue;
                                        }
                                        let ei = EndpointInfo {
                                            fullname: fullname.clone(),
                                            id: ip_port,
                                            name: Some(dn.clone()),
                                            ip: Some(ip.to_string()),
                                            port: Some(port.to_string()),
                                            rtype: Some(dt),
                                            present: Some(true),
                                        };
                                        info!("ServiceResolved: Resolved a new service: {:?}", ei);
                                        cache.insert(fullname.clone(), ei.clone());
                                        let _ = self.sender.send(ei);
                                        break;
                                    }
                                }
                                ServiceEvent::ServiceRemoved(_, fullname) => {
                                    trace!("ServiceRemoved: checking if should remove {}", fullname);
                                    // Only remove if it has not been seen in the last cleanup_threshold
                                    let should_remove = cache.get(&fullname).map(|ei| ei.id.clone());

                                    if let Some(id) = should_remove {
                                        info!("ServiceRemoved: Remove a previous service: {}", fullname);
                                        cache.remove(&fullname);
                                        let _ = self.sender.send(EndpointInfo {
                                            id,
                                            ..Default::default()
                                        });
                                    }
                                }
                                ServiceEvent::SearchStarted(_) | ServiceEvent::SearchStopped(_) => {}
                                _ => {}
                            }
                        },
                        Err(err) => error!("MDnsDiscovery: error: {}", err),
                    }
                }
            }
        }

        Ok(())
    }
}
