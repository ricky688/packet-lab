use std::time::{Duration, Instant};

use anyhow::anyhow;
use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
use btleplug::platform::{Adapter, Manager};
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

const SERVICE_UUID_SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");

const INNER_NAME: &str = "BleListener";

pub struct BleListener {
    adapter: Adapter,
    sender: Sender<()>,
}

impl BleListener {
    pub async fn new(sender: Sender<()>) -> Result<Self, anyhow::Error> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        if adapters.is_empty() {
            return Err(anyhow!("no bluetooth adapter"));
        }

        Ok(Self {
            adapter: adapters[0].clone(),
            sender,
        })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        #[cfg(target_os = "linux")]
        let mut outgoing = crate::bluetooth::outgoing_connections();
        #[cfg(target_os = "linux")]
        let mut active_scan = None;
        let mut events = self.adapter.events().await?;
        let mut scanning = false;
        let mut last_alert: Option<Instant> = None;

        while !ctk.is_cancelled() {
            #[cfg(target_os = "linux")]
            let should_scan = *outgoing.borrow_and_update() == 0;
            #[cfg(not(target_os = "linux"))]
            let should_scan = true;
            if should_scan && !scanning {
                #[cfg(target_os = "linux")]
                {
                    let Some(scan) = crate::bluetooth::DiscoveryScanGuard::start() else { continue };
                    active_scan = Some(scan);
                }
                self.adapter.start_scan(ScanFilter {
                    services: vec![SERVICE_UUID_SHARING],
                }).await?;
                scanning = true;
            } else if !should_scan && scanning {
                self.adapter.stop_scan().await?;
                #[cfg(target_os = "linux")]
                drop(active_scan.take());
                scanning = false;
                debug!("{INNER_NAME}: paused scan during outgoing Bluetooth connection");
            }
            tokio::select! {
                biased;
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                _ = async {
                    #[cfg(target_os = "linux")]
                    let _ = outgoing.changed().await;
                    #[cfg(not(target_os = "linux"))]
                    std::future::pending::<()>().await;
                } => {},
                event = events.next(), if scanning => {
                    let Some(e) = event else { break };
                    match e {
                        CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                            // Sanity check as per: https://github.com/Martichou/rquickshare/issues/74
                            // Seems like the filtering is not enough, so we'll add a check before
                            // proceeding with the service_data.
                            //
                            // ...The filtering is being done only here now.
                            if let Some(service_data) = service_data.get(&SERVICE_UUID_SHARING) {
                                // Quick Share seems to emit LE advert every 10 seconds...
                                // Doesn't really seem much of a spam, so we wait for 10s now
                                // just in case some implementation is sending it under that
                                // time period
                                if last_alert.is_some_and(|last| last.elapsed() <= Duration::from_secs(10)) {
                                    // debug!("{INNER_NAME}: Received LE advert but last alert was {}s ago", now.duration_since(last_alert)?.as_secs());
                                    continue;
                                }

                                debug!("{INNER_NAME}: A device ({id}) is sharing ({}) nearby", hex::encode(service_data));
                                let _ = self.sender.send(());
                                last_alert = Some(Instant::now());
                            }
                        },
                        // Not interesting for us
                        _ => {
                            // trace!("{INNER_NAME}: Another CentralEvent got the same services: {:?}", e);
                        }
                    }
                }
            }
        }

        if scanning {
            self.adapter.stop_scan().await?;
        }
        #[cfg(target_os = "linux")]
        drop(active_scan);
        Ok(())
    }
}
