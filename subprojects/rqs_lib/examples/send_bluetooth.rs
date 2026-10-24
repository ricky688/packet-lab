//! Explicit-peer diagnostic: cargo run -p rqs_lib --example send_bluetooth -- MAC FILE
//! The receiver must still confirm the normal Quick Share request.
use anyhow::{Context, bail};
use rqs_lib::{OutboundPayload, RQS, SendInfo, TransferState, Visibility};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rqs_lib=debug,mdns_sd=warn")
        .init();
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        bail!("usage: send_bluetooth MAC FILE");
    }
    let address: bluer::Address = args[1].parse().context("Invalid Bluetooth address")?;
    let file = PathBuf::from(&args[2]).canonicalize()?;
    if !file.is_file() {
        bail!("Expected a file");
    }
    let id = format!("bluetooth://{address}");
    let mut service = RQS::new(
        Visibility::Invisible,
        Some(9300),
        None,
        Some("Packet Lab".into()),
    );
    let mut events = service.message_sender.subscribe();
    let (requests, _) = service.run().await?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        requests
            .send(SendInfo {
                endpoint_id: None,
                id: id.clone(),
                addr: id.clone(),
                name: "Explicit test phone".into(),
                ob: OutboundPayload::Files(vec![file.to_string_lossy().into_owned()]),
            })
            .await?;
        loop {
            let event = events.recv().await?;
            if event.id != id {
                continue;
            }
            let Some(client) = event.msg.as_client() else {
                continue;
            };
            println!("Transfer state: {:?}", client.state);
            match client.state {
                Some(TransferState::Finished) => return Ok::<_, anyhow::Error>(()),
                Some(
                    TransferState::Disconnected
                    | TransferState::Rejected
                    | TransferState::Cancelled,
                ) => bail!("Transfer did not complete"),
                _ => {}
            }
        }
    })
    .await;
    service.stop().await;
    result.context("Test timed out")?
}
