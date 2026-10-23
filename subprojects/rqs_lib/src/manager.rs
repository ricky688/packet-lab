use tokio::net::TcpListener;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::channel::{self, ChannelMessage, MessageClient, TransferKind};
use crate::connection::Connector;
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, TransferState};
use crate::utils::RemoteDeviceInfo;

const INNER_NAME: &str = "TcpServer";

#[derive(Debug, Clone)]
pub struct SendInfo {
    pub endpoint_id: Option<String>,
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
}

pub struct TcpServer {
    pub(crate) upgrade: Option<crate::bwu::Context>,
    #[cfg(all(feature = "experimental", target_os = "linux"))]
    pub(crate) bluetooth: Option<tokio::sync::mpsc::Sender<crate::bluetooth::Connect>>,
    endpoint_id: [u8; 4],
    connector: Connector,
    tcp_listener: TcpListener,
    sender: Sender<ChannelMessage>,
    connect_receiver: Receiver<SendInfo>,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            upgrade: None,
            #[cfg(all(feature = "experimental", target_os = "linux"))]
            bluetooth: None,
            connector: Connector::from_env()?,
            endpoint_id,
            tcp_listener,
            sender,
            connect_receiver,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        loop {
            let cctk = ctk.clone();

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(i) = self.connect_receiver.recv() => {
                    info!("{INNER_NAME}: connect_receiver: got {:?}", i);
                    let endpoint = self.endpoint_id;
                    let connector = self.connector.clone();
                    let sender = self.sender.clone();
                    let upgrade = self.upgrade.clone();
                    #[cfg(all(feature = "experimental", target_os = "linux"))]
                    let bluetooth = self.bluetooth.clone();
                    tokio::spawn(async move {
                        let id = i.id.clone();
                        if let Err(error) = connect_session(endpoint, connector, sender.clone(), upgrade,
                            #[cfg(all(feature = "experimental", target_os = "linux"))] bluetooth,
                            cctk, i).await {
                            error!("{INNER_NAME}: error sending: {error}");
                            let _ = sender.send(ChannelMessage { id, msg: channel::Message::Client(MessageClient {
                                kind: TransferKind::Outbound, state: Some(TransferState::Disconnected), metadata: None,
                            }) });
                        }
                    });
                }
                r = self.tcp_listener.accept() => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            trace!("{INNER_NAME}: new client: {remote_addr}");
                            let sender = self.sender.clone();
                            let token = ctk.clone();
                            let upgrade = self.upgrade.clone();
                            tokio::spawn(async move { receive(socket, remote_addr.to_string(), sender, token, upgrade, false).await });
                        },
                        Err(err) => {
                            error!("{INNER_NAME}: error accepting: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

async fn connect_session(
    endpoint_id: [u8; 4],
    connector: Connector,
    sender: Sender<ChannelMessage>,
    upgrade: Option<crate::bwu::Context>,
    #[cfg(all(feature = "experimental", target_os = "linux"))] bluetooth: Option<
        tokio::sync::mpsc::Sender<crate::bluetooth::Connect>,
    >,
    ctk: CancellationToken,
    si: SendInfo,
) -> anyhow::Result<()> {
    debug!("{INNER_NAME}: Connecting to: {}", si.addr);
    let attempt = async {
        #[cfg(all(feature = "experimental", target_os = "linux"))]
        if si.addr.starts_with("bluetooth://") {
            return crate::bluetooth::connect(
                bluetooth
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Bluetooth service unavailable"))?,
                &si.addr,
            )
            .await;
        }
        Ok::<_, anyhow::Error>(crate::transport::Transport::new(
            connector.connect(&si.addr).await?,
        ))
    };
    let socket = tokio::select! {
        _ = ctk.cancelled() => return Ok(()),
        result = attempt => result?,
    };

    let mut or = OutboundRequest::new(
        endpoint_id,
        socket,
        si.id,
        sender.clone(),
        si.ob,
        RemoteDeviceInfo {
            device_type: crate::DeviceType::Unknown,
            name: si.name,
        },
    );

    or.upgrade.context = upgrade;
    or.upgrade.remote = si.endpoint_id;
    or.upgrade.bluetooth = si.addr.starts_with("bluetooth://");
    // Send connection request
    or.send_connection_request().await?;
    // Send UKEY init
    or.send_ukey2_client_init().await?;

    loop {
        tokio::select! {
            _ = ctk.cancelled() => {
                info!("{INNER_NAME}: tracker cancelled, breaking");
                break;
            },
            r = or.handle() => {
                if let Err(e) = r {
                    match e.downcast_ref() {
                        Some(AppError::NotAnError) => break,
                        None => {
                            if or.state.state == TransferState::Initial {
                                break;
                            }

                            if or.state.state != TransferState::Finished && or.state.state != TransferState::Cancelled {
                                let _ = sender.clone().send(ChannelMessage {
                                    id: si.addr,
                                    msg: channel::Message::Client(MessageClient {
                                        kind: TransferKind::Outbound,
                                        state: Some(TransferState::Disconnected),
                                        metadata: Default::default()
                                    }),
                                });
                            }
                            error!("{INNER_NAME}: error while handling client: {e} ({:?})", or.state.state);
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn receive(
    socket: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
    id: String,
    sender: Sender<ChannelMessage>,
    token: CancellationToken,
    upgrade: Option<crate::bwu::Context>,
    bluetooth: bool,
) {
    let mut request = InboundRequest::new(socket, id.clone(), sender.clone());
    request.upgrade.context = upgrade;
    request.upgrade.bluetooth = bluetooth;
    loop {
        let result = tokio::select! {
            _ = token.cancelled() => break,
            result = request.handle() => result,
        };
        if let Err(error) = result {
            if error.downcast_ref::<AppError>().is_none()
                && request.state.state != TransferState::Finished
            {
                let _ = sender.send(ChannelMessage {
                    id: id.clone(),
                    msg: channel::Message::Client(MessageClient {
                        kind: TransferKind::Inbound,
                        state: Some(TransferState::Disconnected),
                        metadata: None,
                    }),
                });
                warn!("Incoming session {id}: {error}");
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{broadcast, mpsc};

    #[tokio::test]
    async fn pending_outgoing_handshake_does_not_block_upgrade_listener() {
        let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, _events) = broadcast::channel(10);
        let (requests, receiver) = mpsc::channel(1);
        let mut server = TcpServer::new(*b"TEST", listener, sender, receiver).unwrap();
        let token = CancellationToken::new();
        let child = token.clone();
        let task = tokio::spawn(async move { server.run(child).await });
        requests
            .send(SendInfo {
                endpoint_id: None,
                id: "outbound".into(),
                name: "peer".into(),
                addr: peer_listener.local_addr().unwrap().to_string(),
                ob: OutboundPayload::Files(vec![]),
            })
            .await
            .unwrap();
        let (_stalled_peer, _) = peer_listener.accept().await.unwrap();
        let mut incoming = tokio::net::TcpStream::connect(address).await.unwrap();
        // An empty protobuf has no connection request: the listener must reject it promptly.
        incoming.write_u32(0).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), incoming.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        token.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connection_failure_reports_disconnected() {
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = unused.local_addr().unwrap().to_string();
        drop(unused);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (sender, mut events) = broadcast::channel(10);
        let (requests, receiver) = mpsc::channel(1);
        let mut server = TcpServer::new(*b"TEST", listener, sender, receiver).unwrap();
        let token = CancellationToken::new();
        let child = token.clone();
        let task = tokio::spawn(async move { server.run(child).await });
        requests
            .send(SendInfo {
                endpoint_id: None,
                id: "manual-test".into(),
                name: "Synthetic peer".into(),
                addr: address,
                ob: OutboundPayload::Files(vec![]),
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "manual-test");
        assert_eq!(
            event.msg.as_client().unwrap().state,
            Some(TransferState::Disconnected)
        );
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
