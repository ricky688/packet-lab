use futures_lite::StreamExt;
use tokio::sync::watch;
use zbus::zvariant::OwnedObjectPath;

const BLUEZ_ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";

pub async fn bluez_restarts(
    conn: &zbus::Connection,
) -> zbus::Result<impl futures_lite::Stream<Item = ()> + 'static> {
    let bus = zbus::fdo::DBusProxy::new(conn).await?;
    let changes = bus.receive_name_owner_changed_with_args(&[(0, "org.bluez")]).await?;
    Ok(changes.filter_map(|change| {
        let args = change.args().ok()?;
        args.new_owner().as_ref()?;
        Some(())
    }))
}

/// Finds the object path of a Bluetooth adapter (e.g. `/org/bluez/hci0`) by asking BlueZ
/// for its managed objects, rather than assuming `hci0` always exists: some systems only
/// expose `hci1` or other indices.
async fn find_adapter_path(conn: &zbus::Connection) -> zbus::Result<OwnedObjectPath> {
    let object_manager = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;

    object_manager
        .get_managed_objects()
        .await?
        .into_iter()
        .filter(|(_, interfaces)| {
            interfaces
                .keys()
                .any(|interface| interface.as_str() == BLUEZ_ADAPTER_INTERFACE)
        })
        .map(|(path, _)| path)
        .min_by(|a, b| a.as_str().cmp(b.as_str()))
        .ok_or_else(|| zbus::Error::Failure("No Bluetooth adapter found".to_string()))
}

pub async fn spawn_bluetooth_power_monitor_task(
    conn: zbus::Connection,
    sender: watch::Sender<bool>,
) -> zbus::Result<()> {
    let object_manager = zbus::fdo::ObjectManagerProxy::builder(&conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;

    let mut interfaces_added = object_manager.receive_interfaces_added().await?;
    let mut interfaces_removed = object_manager.receive_interfaces_removed().await?;

    let mut current_adapter: Option<(OwnedObjectPath, zbus::proxy::PropertyStream<bool>)> = None;

    let new_current_adapter = async |path: OwnedObjectPath| {
        let proxy = zbus::Proxy::new(&conn, "org.bluez", path.clone(), BLUEZ_ADAPTER_INTERFACE)
            .await
            .ok()?;
        let powered_stream = proxy.receive_property_changed::<bool>("Powered").await;
        if let Ok(powered) = proxy.get_property::<bool>("Powered").await {
            _ = sender.send(powered);
        }
        Some((path, powered_stream))
    };

    if let Ok(path) = find_adapter_path(&conn).await {
        current_adapter = new_current_adapter(path).await;
    } else {
        _ = sender.send(false);
    }

    loop {
        tokio::select! {
            Some(added) = interfaces_added.next() => {
                if let Ok(args) = added.args() && args.interfaces_and_properties().contains_key(BLUEZ_ADAPTER_INTERFACE) {
                    let path: OwnedObjectPath = args.object_path().to_owned().into();
                    current_adapter = new_current_adapter(path).await;
                }
            }
            Some(removed) = interfaces_removed.next() => {
                if let Ok(args) = removed.args() {
                    let should_reset = if let Some((current_path, _)) = &current_adapter {
                        args.object_path().as_str() == current_path.as_str()
                            || args.interfaces().iter().any(|i| i.as_str() == BLUEZ_ADAPTER_INTERFACE)
                    } else {
                        false
                    };

                    if should_reset {
                        current_adapter = None;
                        _ = sender.send(false);

                        if let Ok(new_path) = find_adapter_path(&conn).await {
                            current_adapter = new_current_adapter(new_path).await;
                        }
                    }
                }
            }
            event = async {
                match &mut current_adapter {
                    Some((_, powered_stream)) => powered_stream.next().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(event) = event && let Ok(powered) = event.get().await {
                    _ = sender.send(powered);
                }
            }
        }
    }
}

pub async fn is_bluetooth_powered(conn: &zbus::Connection) -> zbus::Result<bool> {
    let adapter_path = find_adapter_path(conn).await?;
    let proxy = zbus::Proxy::new(conn, "org.bluez", adapter_path, BLUEZ_ADAPTER_INTERFACE).await?;

    let value: bool = proxy.get_property("Powered").await?;

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    struct TestBus(Child);
    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn daemon_replacement_emits_recovery_without_a_power_change() {
        let mut daemon = TestBus(Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped()).spawn().unwrap());
        let mut address = String::new();
        std::io::BufReader::new(daemon.0.stdout.take().unwrap())
            .read_line(&mut address).unwrap();
        let observer = zbus::connection::Builder::address(address.trim()).unwrap()
            .build().await.unwrap();
        let first = zbus::connection::Builder::address(address.trim()).unwrap()
            .build().await.unwrap();
        first.request_name("org.bluez").await.unwrap();
        let mut restarts = bluez_restarts(&observer).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), restarts.next()).await.is_err());
        first.close().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), restarts.next()).await.is_err());
        let replacement = zbus::connection::Builder::address(address.trim()).unwrap()
            .build().await.unwrap();
        replacement.request_name("org.bluez").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), restarts.next())
            .await.unwrap().is_some());
    }
}
