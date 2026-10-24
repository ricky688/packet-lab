use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use libaes::{AES_256_KEY_LEN, Cipher};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey};
use prost::Message;
use rand::Rng;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(test)]
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::time::Instant;

use super::info::{InternalFileInfo, TransferMetadata, TransferPayload, TransferPayloadKind};
use super::{InnerState, TextPayloadInfo, TextPayloadType, TransferState};
use crate::channel::{self, ChannelMessage, MessageClient, TransferAction, TransferKind};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
use crate::location_nearby_connections::connection_response_frame::ResponseStatus;
use crate::location_nearby_connections::payload_transfer_frame::{
    PacketType, PayloadChunk, PayloadHeader, payload_header,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::ukey2_client_init::CipherCommitment;
use crate::securegcm::{
    DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished, Ukey2ClientInit,
    Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit, ukey2_message,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::sharing_nearby::{
    FileMetadata, IntroductionFrame, TextMetadata, file_metadata, paired_key_result_frame,
};
use crate::utils::{
    DeviceType, RemoteDeviceInfo, create_text_preview, encode_point, gen_ecdsa_keypair, gen_random,
    hkdf_extract_expand, stream_read_exact, to_four_digit_string,
};
use crate::{DEVICE_NAME, location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    Files(Vec<String>),
    Text {
        text: String,
        /// Doesn't support WiFi outbound payload yet due to default SSID and SecurityType
        text_type: TextPayloadType,
    },
}

#[derive(Debug)]
pub struct OutboundRequest {
    pub(crate) upgrade: crate::bwu::Upgrade,
    endpoint_id: [u8; 4],
    socket: crate::transport::Transport,
    pub state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    payload: OutboundPayload,
    completion_deadline: Option<Instant>,
    deferred_consent: Option<sharing_nearby::V1Frame>,
}

impl OutboundRequest {
    pub fn new(
        endpoint_id: [u8; 4],
        socket: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
        id: String,
        sender: Sender<ChannelMessage>,
        payload: OutboundPayload,
        rdi: RemoteDeviceInfo,
    ) -> Self {
        let receiver = sender.subscribe();

        let transfer_metadata = match &payload {
            OutboundPayload::Files(files) => TransferMetadata {
                source: Some(rdi),
                payload_kind: TransferPayloadKind::Files,
                payload: Some(TransferPayload::Files(files.to_vec())),
                id: Default::default(),
                pin_code: Default::default(),
                payload_preview: Default::default(),
                total_bytes: Default::default(),
                ack_bytes: Default::default(),
                transport_medium: Default::default(),
            },
            OutboundPayload::Text { text, text_type } => {
                let (payload_kind, transfer_payload) = match text_type {
                    TextPayloadType::Url => {
                        (TransferPayloadKind::Url, TransferPayload::Url(text.clone()))
                    }
                    TextPayloadType::Text => (
                        TransferPayloadKind::Text,
                        TransferPayload::Text(text.clone()),
                    ),
                    TextPayloadType::Wifi => (
                        TransferPayloadKind::Text,
                        TransferPayload::Text(text.clone()),
                    ),
                };
                TransferMetadata {
                    source: Some(rdi),
                    payload_kind,
                    payload: Some(transfer_payload),
                    id: Default::default(),
                    pin_code: Default::default(),
                    payload_preview: Some(create_text_preview(text, 32).clone()),
                    total_bytes: text.len() as u64,
                    ack_bytes: Default::default(),
                    transport_medium: Default::default(),
                }
            }
        };

        Self {
            upgrade: Default::default(),
            endpoint_id,
            socket: crate::transport::Transport::new(socket),
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: TransferState::Initial,
                encryption_done: true,
                transfer_metadata: Some(transfer_metadata),
                ..Default::default()
            },
            sender,
            receiver,
            payload,
            completion_deadline: None,
            deferred_consent: None,
        }
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        if self.upgrade.lan_failed {
            if self.upgrade.context.is_some()
                && !self.upgrade.direct_failed
                && self.upgrade.waiting.is_none()
                && !self.upgrade.last_sent
            {
                if let Ok(Some(offer)) = self.upgrade.offer() {
                    info!("LAN upgrade failed; sender falling back to Wi-Fi Direct offer");
                    self.encrypt_and_send(&offer).await?;
                }
            }
            if let Some(consent) = self.deferred_consent.take() {
                return self.process_consent(&consent).await;
            }
        }
        // Buffer for the 4-byte length
        let mut length_buf = [0u8; 4];

        let completion_deadline = self.completion_deadline;
        let upgrade_deadline = self.upgrade.deadline;
        tokio::select! {
            _ = async {
                match upgrade_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => return Err(anyhow!("Wi-Fi handover timed out")),
            _ = async {
                match completion_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => return Err(anyhow!("Receiver did not finish the transfer within 30 seconds")),
            i = self.receiver.recv() => {
                match i {
                    Ok(channel_msg) => {
                        if channel_msg.id != self.state.id {
                            return Ok(());
                        }

                        if let channel::Message::Lib { action }  = &channel_msg.msg {
                            debug!("outbound: got: {:?}", channel_msg);
                            match action {
                                TransferAction::TransferCancel => {
                                    self.update_state(
                                        |e| {
                                            e.state = TransferState::Cancelled;
                                        },
                                        true,
                                    ).await;
                                    self.disconnection().await?;
                                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                                },
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            first = self.socket.read(&mut length_buf[..1]) => {
                let first = first?;
                let read_frame = async {
                    // Channel events must not discard a partially read frame header.
                    if first == 0 {
                        if self.completion_deadline.is_some() {
                            return self.receiver_completed().await;
                        }
                        return Err(anyhow!("Receiver closed before all payloads were sent"));
                    }
                    stream_read_exact(&mut self.socket, &mut length_buf[1..]).await?;
                    self._handle(length_buf).await
                };
                if let Some(deadline) = completion_deadline.into_iter().chain(upgrade_deadline).min() {
                    tokio::time::timeout_at(deadline, read_frame).await
                        .map_err(|_| if Some(deadline) == upgrade_deadline { anyhow!("Wi-Fi handover timed out") } else { anyhow!("Receiver did not finish the transfer within 30 seconds") })??;
                } else {
                    read_frame.await?;
                }
            }
        }

        Ok(())
    }

    pub async fn _handle(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let msg_length = u32::from_be_bytes(length_buf) as usize;
        // Ensure the message length is not unreasonably big to avoid allocation attacks
        if msg_length > SANE_FRAME_LENGTH as usize {
            error!("Message length too big");
            return Err(anyhow!("value"));
        }

        // Allocate buffer for the actual message and read it
        let mut frame_data = vec![0u8; msg_length];
        stream_read_exact(&mut self.socket, &mut frame_data).await?;

        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            TransferState::SentUkeyClientInit => {
                debug!("Handling State::SentUkeyClientInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.update_state(
                    |e| {
                        e.server_init_data = Some(frame_data);
                    },
                    false,
                )
                .await;
                self.process_ukey2_server_init(&msg).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentUkeyClientFinish;
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            TransferState::SentUkeyClientFinish => {
                debug!("Handling State::SentUkeyClientFinish frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                self.process_connection_response(&frame).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentPairedKeyEncryption;
                        e.server_init_data = Some(frame_data);
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            _ => {
                debug!("Handling SecureMessage frame");
                let smsg = SecureMessage::decode(&*frame_data)?;
                self.decrypt_and_process_secure_message(&smsg).await?;
            }
        }

        Ok(())
    }

    pub async fn send_connection_request(&mut self) -> Result<(), anyhow::Error> {
        let device_name = DEVICE_NAME.read().unwrap().clone();
        let request = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionRequest.into(),
                ),
                connection_request: Some(location_nearby_connections::ConnectionRequestFrame {
                    endpoint_id: Some(String::from_utf8_lossy(&self.endpoint_id).to_string()),
                    endpoint_name: Some(device_name.clone().into()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: device_name.clone().into(),
                            device_type: DeviceType::Laptop,
                        }
                        .serialize(),
                    ),
                    mediums: vec![
                        Medium::WifiLan.into(),
                        Medium::WifiDirect.into(),
                        Medium::WifiHotspot.into(),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_frame(request.encode_to_vec()).await?;

        Ok(())
    }

    pub async fn send_ukey2_client_init(&mut self) -> Result<(), anyhow::Error> {
        let (secret_key, public_key) = gen_ecdsa_keypair();

        let encoded_point = public_key.to_encoded_point(false);
        let x = encoded_point.x().unwrap();
        let y = encoded_point.y().unwrap();

        let pkey = GenericPublicKey {
            r#type: PublicKeyType::EcP256.into(),
            ec_p256_public_key: Some(EcP256PublicKey {
                x: encode_point(Bytes::from(x.to_vec()))?,
                y: encode_point(Bytes::from(y.to_vec()))?,
            }),
            ..Default::default()
        };

        let finish_frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientFinish.into()),
            message_data: Some(
                Ukey2ClientFinished {
                    public_key: Some(pkey.encode_to_vec()),
                }
                .encode_to_vec(),
            ),
        };

        let sha512 = Sha512::digest(finish_frame.encode_to_vec());
        let frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit.into()),
            message_data: Some(
                Ukey2ClientInit {
                    version: Some(1),
                    random: Some(gen_random(32)),
                    next_protocol: Some(String::from("AES_256_CBC-HMAC_SHA256")),
                    cipher_commitments: vec![CipherCommitment {
                        handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
                        commitment: Some(sha512.to_vec()),
                    }],
                }
                .encode_to_vec(),
            ),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        self.update_state(
            |e| {
                e.state = TransferState::SentUkeyClientInit;
                e.private_key = Some(secret_key);
                e.public_key = Some(public_key);
                e.client_init_msg_data = Some(frame.encode_to_vec());
                e.ukey_client_finish_msg_data = Some(finish_frame.encode_to_vec());
            },
            false,
        )
        .await;

        Ok(())
    }

    async fn process_ukey2_server_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ServerInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ServerInit",
                msg.message_type
            ));
        }

        let server_init = match Ukey2ServerInit::decode(msg.message_data()) {
            Ok(uk2si) => uk2si,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if server_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: server_init.version != 1"));
        }

        if server_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: server_init.random.len != 32"));
        }

        if server_init.handshake_cipher() != Ukey2HandshakeCipher::P256Sha512 {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: handshake_cipher != P256Sha512"));
        }

        let server_public_key = match GenericPublicKey::decode(server_init.public_key()) {
            Ok(spk) => spk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(server_public_key).await?;
        self.send_frame(self.state.ukey_client_finish_msg_data.clone().unwrap())
            .await?;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into(),
                ),
                connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(location_nearby_connections::os_info::OsType::Linux.into())
					}),
					..Default::default()
				}),
                ..Default::default()
            }),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        Ok(())
    }

    async fn process_connection_response(
        &mut self,
        frame: &location_nearby_connections::OfflineFrame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() != location_nearby_connections::v1_frame::FrameType::ConnectionResponse
        {
            return Err(anyhow!(format!(
                "Unexpected frame type: {:?}",
                v1_frame.r#type()
            )));
        }

        if v1_frame.connection_response.is_none() {
            return Err(anyhow!(format!("Unexpected None connection_response",)));
        }

        if v1_frame.connection_response.as_ref().unwrap().response() != ResponseStatus::Accept {
            return Err(anyhow!(format!("Connection rejected by third party",)));
        }

        let paired_encryption = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyEncryption.into()),
                paired_key_encryption: Some(sharing_nearby::PairedKeyEncryptionFrame {
                    secret_id_hash: Some(gen_random(6)),
                    signed_data: Some(gen_random(72)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&paired_encryption).await?;
        // The advertising peer (receiver) initiates Nearby's bandwidth upgrade after consent.
        if self.upgrade.bluetooth {
            self.upgrade.deadline = Some(Instant::now() + Duration::from_secs(60));
        }

        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let mut hmac = HmacSha256::new_from_slice(self.state.recv_hmac_key.as_ref().unwrap())?;
        hmac.update(&smsg.header_and_body);
        if !hmac
            .finalize()
            .into_bytes()
            .as_slice()
            .eq(smsg.signature.as_slice())
        {
            return Err(anyhow!("hmac!=signature"));
        }

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = self.state.decrypt_key.as_ref().unwrap();

        let mut cipher = Cipher::new_256(key[..AES_256_KEY_LEN].try_into()?);
        cipher.set_auto_padding(true);
        let decrypted = cipher.cbc_decrypt(header_and_body.header.iv(), &msg_data);

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;

        let seq = self.get_client_seq_inc().await;
        if d2d_msg.sequence_number() != seq {
            return Err(anyhow!(
                "Error d2d_msg.sequence_number invalid ({} vs {})",
                d2d_msg.sequence_number(),
                seq
            ));
        }

        let offline = location_nearby_connections::OfflineFrame::decode(d2d_msg.message())?;
        self.process_offline_frame(&offline).await
    }

    async fn process_offline_frame(&mut self, offline: &OfflineFrame) -> Result<(), anyhow::Error> {
        let v1_frame = offline
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;
        if v1_frame.r#type()
            == location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
        {
            let packet = v1_frame
                .bandwidth_upgrade_negotiation
                .as_ref()
                .ok_or_else(|| anyhow!("Missing upgrade negotiation"))?;
            match self.upgrade.handle(packet).await? {
                crate::bwu::Action::Send(frames) => {
                    for frame in frames {
                        self.encrypt_and_send(&frame).await?;
                    }
                }
                crate::bwu::Action::Switch => {
                    self.upgrade.switch(&mut self.socket).await?;
                    for frame in std::mem::take(&mut self.upgrade.deferred) {
                        self.encrypt_and_send(&frame).await?;
                    }
                    if let Some(consent) = self.deferred_consent.take() {
                        self.process_consent(&consent).await?;
                    }
                }
            }
            return Ok(());
        }
        match v1_frame.r#type() {
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer => {
                trace!("Received FrameType::PayloadTransfer");
                let payload_transfer = v1_frame
                    .payload_transfer
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                let header = payload_transfer
                    .payload_header
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;
                if payload_transfer.packet_type() == PacketType::Control {
                    let control = payload_transfer
                        .control_message
                        .as_ref()
                        .ok_or_else(|| anyhow!("Missing payload control message"))?;
                    let known = self.state.transferred_files.contains_key(&header.id())
                        || self
                            .state
                            .text_payload
                            .as_ref()
                            .is_some_and(|payload| payload.get_i64_value() == header.id());
                    if known {
                        use crate::location_nearby_connections::payload_transfer_frame::control_message::EventType;
                        match control.event() {
                            EventType::PayloadError | EventType::PayloadCanceled => {
                                return Err(anyhow!(
                                    "Receiver rejected payload {}: {:?}",
                                    header.id(),
                                    control.event()
                                ));
                            }
                            EventType::PayloadReceivedAck => {
                                debug!("Receiver acknowledged payload {}", header.id());
                            }
                            EventType::UnknownEventType => {}
                        }
                    }
                    return Ok(());
                }
                let chunk = payload_transfer
                    .payload_chunk
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                match header.r#type() {
                    payload_header::PayloadType::Bytes => {
                        info!("Processing PayloadType::Bytes");
                        let payload_id = header.id();

                        if header.total_size() > SANE_FRAME_LENGTH.into() {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Payload too large: {} bytes",
                                header.total_size()
                            ));
                        }

                        self.state
                            .payload_buffers
                            .entry(payload_id)
                            .or_insert_with(|| Vec::with_capacity(header.total_size() as usize));

                        // Get the current length of the buffer, if it exists, without holding a mutable borrow.
                        let buffer_len = self.state.payload_buffers.get(&payload_id).unwrap().len();
                        if chunk.offset() != buffer_len as i64 {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Unexpected chunk offset: {}, expected: {}",
                                chunk.offset(),
                                buffer_len
                            ));
                        }

                        let buffer = self.state.payload_buffers.get_mut(&payload_id).unwrap();
                        if let Some(body) = &chunk.body {
                            buffer.extend(body);
                        }

                        if (chunk.flags() & 1) == 1 {
                            debug!("Chunk flags & 1 == 1 ?? End of data ??");

                            let inner_frame = sharing_nearby::Frame::decode(buffer.as_slice())?;
                            self.process_transfer_setup(&inner_frame).await?;
                        }
                    }
                    payload_header::PayloadType::File => {
                        error!("Unhandled PayloadType::File: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::Stream => {
                        error!("Unhandled PayloadType::Stream: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::UnknownPayloadType => {
                        error!(
                            "Invalid PayloadType::UnknownPayloadType: {:?}",
                            header.r#type()
                        )
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::Disconnection => {
                if self.completion_deadline.is_some() {
                    return self.receiver_completed().await;
                }
                return Err(anyhow!(
                    "Receiver disconnected before all payloads were sent"
                ));
            }
            location_nearby_connections::v1_frame::FrameType::KeepAlive => {
                trace!("Sending keepalive");
                self.send_keepalive(true).await?;
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry => {
                if let Some(retry) = &v1_frame.bandwidth_upgrade_retry {
                    self.process_bandwidth_upgrade_retry(retry).await?;
                }
            }
            _ => {
                error!("Unhandled offline frame encrypted: {:?}", offline);
            }
        }

        Ok(())
    }

    pub(crate) async fn process_bandwidth_upgrade_retry(
        &mut self,
        retry: &location_nearby_connections::BandwidthUpgradeRetryFrame,
    ) -> Result<(), anyhow::Error> {
        info!(
            "Received BandwidthUpgradeRetry: is_request={:?}, supported_medium={:?}",
            retry.is_request, retry.supported_medium
        );
        if retry.is_request() {
            let response = location_nearby_connections::OfflineFrame {
                version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
                v1: Some(location_nearby_connections::V1Frame {
                    r#type: Some(
                        location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry
                            .into(),
                    ),
                    bandwidth_upgrade_retry: Some(
                        location_nearby_connections::BandwidthUpgradeRetryFrame {
                            is_request: Some(false),
                            supported_medium: vec![
                                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiHotspot
                                    .into(),
                                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiDirect
                                    .into(),
                                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiLan.into(),
                            ],
                        },
                    ),
                    ..Default::default()
                }),
            };
            self.encrypt_and_send(&response).await?;

            if self.upgrade.bluetooth && !self.upgrade.complete {
                if self.upgrade.context.is_some() {
                    if let Some(offer) = self.upgrade.offer()? {
                        info!("Bluetooth -> Wi-Fi: sender offering upgrade path after retry request");
                        self.encrypt_and_send(&offer).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn process_transfer_setup(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() == sharing_nearby::v1_frame::FrameType::Cancel {
            info!("Transfer canceled");
            self.update_state(
                |e| {
                    e.state = TransferState::Cancelled;
                },
                true,
            )
            .await;
            self.disconnection().await?;
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }

        match self.state.state {
            TransferState::SentPairedKeyEncryption => {
                debug!("Processing State::SentPairedKeyEncryption");
                self.process_paired_key_encryption_frame(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = TransferState::SentPairedKeyResult;
                    },
                    false,
                )
                .await;
            }
            TransferState::SentPairedKeyResult => {
                debug!("Processing State::SentPairedKeyResult");
                self.process_paired_key_result(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = TransferState::SentIntroduction;
                    },
                    true,
                )
                .await;
            }
            TransferState::SentIntroduction => {
                debug!("Processing State::SentIntroduction");
                self.process_consent(v1_frame).await?;
            }
            TransferState::SendingFiles => {}
            _ => {
                info!(
                    "Unhandled connection state in process_transfer_setup: {:?}",
                    self.state.state
                );
            }
        }

        Ok(())
    }

    async fn process_paired_key_encryption_frame(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_encryption.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        let paired_result = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyResult.into()),
                paired_key_result: Some(sharing_nearby::PairedKeyResultFrame {
                    status: Some(paired_key_result_frame::Status::Unable.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&paired_result).await?;

        Ok(())
    }

    async fn process_paired_key_result(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        let mut file_metadata: Vec<FileMetadata> = vec![];
        let mut text_metadata: Vec<TextMetadata> = vec![];
        let mut transferred_files: HashMap<i64, InternalFileInfo> = HashMap::new();
        let mut total_to_send: u64 = 0;
        let mut text_payload_info: Option<TextPayloadInfo> = None;

        match &self.payload {
            OutboundPayload::Files(files) => {
                for f in files {
                    let path = Path::new(f);
                    if !path.is_file() {
                        warn!("Path is not a file: {}", f);
                        continue;
                    }

                    let file = match File::open(f) {
                        Ok(_f) => _f,
                        Err(e) => {
                            error!("Failed to open file: {f}: {:?}", e);
                            continue;
                        }
                    };
                    let fmetadata = match file.metadata() {
                        Ok(_fm) => _fm,
                        Err(e) => {
                            error!("Failed to get metadata for: {f}: {:?}", e);
                            continue;
                        }
                    };

                    let ftype = mime_guess::from_path(path)
                        .first_or_octet_stream()
                        .to_string();

                    let meta_type = if ftype.starts_with("image/") {
                        file_metadata::Type::Image
                    } else if ftype.starts_with("video/") {
                        file_metadata::Type::Video
                    } else if ftype.starts_with("audio/") {
                        file_metadata::Type::Audio
                    } else if path.extension().unwrap_or_default() == "apk" {
                        file_metadata::Type::App
                    } else {
                        file_metadata::Type::Unknown
                    };

                    info!("File type to send: {}", ftype);
                    let fname = path
                        .file_name()
                        .ok_or_else(|| anyhow!("Failed to get file_name for {f}"))?;
                    let fmeta = FileMetadata {
                        id: Some(file_metadata.len() as i64 + 1),
                        payload_id: Some(rand::rng().random::<i64>()),
                        name: Some(fname.to_os_string().into_string().unwrap()),
                        size: Some(fmetadata.size() as i64),
                        mime_type: Some(ftype),
                        r#type: Some(meta_type.into()),
                        ..Default::default()
                    };
                    transferred_files.insert(
                        fmeta.payload_id(),
                        InternalFileInfo {
                            payload_id: fmeta.payload_id(),
                            file_url: path.to_path_buf(),
                            bytes_transferred: 0,
                            total_size: fmeta.size(),
                            file: Some(file),
                        },
                    );
                    file_metadata.push(fmeta);
                    total_to_send += fmetadata.size();
                }
            }
            OutboundPayload::Text { text, text_type } => {
                let payload_id = rand::rng().random::<i64>();
                let text_bytes = text.as_bytes();
                total_to_send = text_bytes.len() as u64;

                text_payload_info = Some(TextPayloadInfo::from_type(text_type, payload_id));

                let tmeta = TextMetadata {
                    text_title: None,
                    r#type: Some(text_type.to_proto_type().into()),
                    payload_id: Some(payload_id),
                    size: Some(text_bytes.len() as i64),
                    id: Some(payload_id),
                };
                text_metadata.push(tmeta);

                info!(
                    "Text type to send: {:?}, size: {}",
                    text_type,
                    text_bytes.len()
                );
            }
        }

        self.update_state(
            |e| {
                if let Some(tmd) = e.transfer_metadata.as_mut() {
                    tmd.total_bytes = total_to_send;
                }
                e.transferred_files = transferred_files;
                if text_payload_info.is_some() {
                    e.text_payload = text_payload_info;
                }
            },
            false,
        )
        .await;

        let intro_frame = if text_metadata.is_empty() {
            IntroductionFrame {
                file_metadata,
                ..Default::default()
            }
        } else {
            IntroductionFrame {
                file_metadata,
                text_metadata,
                ..Default::default()
            }
        };

        let introduction = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Introduction.into()),
                introduction: Some(intro_frame),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&introduction).await?;

        Ok(())
    }

    async fn process_consent(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.r#type() != sharing_nearby::v1_frame::FrameType::Response
            || v1_frame.connection_response.is_none()
        {
            return Err(anyhow!("Missing required fields"));
        }

        match v1_frame.connection_response.as_ref().unwrap().status() {
            sharing_nearby::connection_response_frame::Status::Accept => {
                if self.upgrade.bluetooth
                    && !self.upgrade.complete
                    && (self.upgrade.last_sent
                        || (!self.upgrade.lan_failed
                            && self
                                .state
                                .transfer_metadata
                                .as_ref()
                                .is_some_and(|m| m.total_bytes >= 1_000_000)))
                {
                    self.deferred_consent = Some(v1_frame.clone());
                    if !self.upgrade.last_sent && self.upgrade.waiting.is_none() {
                        if let Some(offer) = self.upgrade.offer()? {
                            info!("Bluetooth -> Wi-Fi: sender proactively proposing bandwidth upgrade offer after consent");
                            self.encrypt_and_send(&offer).await?;
                        }
                    }
                    return Ok(());
                }
                if self.upgrade.bluetooth && !self.upgrade.complete {
                    // Small payloads and failed LAN upgrades retain the encrypted Bluetooth channel.
                    self.upgrade.deadline = None;
                }
                info!("State is now State::SendingFiles");
                self.update_state(
                    |e| {
                        e.state = TransferState::SendingFiles;
                    },
                    true,
                )
                .await;

                // Handle sending Text
                if let Some(ref text_payload_info) = self.state.text_payload {
                    let payload_id = text_payload_info.get_i64_value();

                    if let OutboundPayload::Text { ref text, .. } = self.payload {
                        let text_bytes = text.as_bytes().to_vec();
                        let text_len = text_bytes.len() as i64;

                        info!("Sending text payload, size: {}", text_len);

                        let payload_header = PayloadHeader {
                            id: Some(payload_id),
                            r#type: Some(payload_header::PayloadType::Bytes.into()),
                            total_size: Some(text_len),
                            is_sensitive: Some(false),
                            ..Default::default()
                        };

                        // Send the text content
                        let wrapper = location_nearby_connections::OfflineFrame {
                            version: Some(
                                location_nearby_connections::offline_frame::Version::V1.into(),
                            ),
                            v1: Some(location_nearby_connections::V1Frame {
                                r#type: Some(
                                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                        .into(),
                                ),
                                payload_transfer: Some(PayloadTransferFrame {
                                    packet_type: Some(PacketType::Data.into()),
                                    payload_chunk: Some(PayloadChunk {
                                        offset: Some(0),
                                        flags: Some(0),
                                        body: Some(text_bytes.clone()),
                                    }),
                                    payload_header: Some(payload_header.clone()),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                        };

                        self.encrypt_and_send(&wrapper).await?;

                        // Send lastChunk marker
                        let wrapper = location_nearby_connections::OfflineFrame {
                            version: Some(
                                location_nearby_connections::offline_frame::Version::V1.into(),
                            ),
                            v1: Some(location_nearby_connections::V1Frame {
                                r#type: Some(
                                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                        .into(),
                                ),
                                payload_transfer: Some(PayloadTransferFrame {
                                    packet_type: Some(PacketType::Data.into()),
                                    payload_chunk: Some(PayloadChunk {
                                        offset: Some(text_len),
                                        flags: Some(1), // lastChunk
                                        body: Some(vec![]),
                                    }),
                                    payload_header: Some(payload_header),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                        };

                        self.encrypt_and_send(&wrapper).await?;

                        // Update progress
                        self.update_state(
                            |e| {
                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                    tmd.ack_bytes = text_len as u64;
                                }
                            },
                            true,
                        )
                        .await;

                        self.wait_for_receiver_completion();
                        return Ok(());
                    }
                }

                // Handle sending Files
                let ids: Vec<i64> = self.state.transferred_files.keys().cloned().collect();
                info!("We are sending: {:?}", ids);
                let mut ids_iter = ids.into_iter();

                // Loop through all files
                'send_all_files: loop {
                    let current = match ids_iter.next() {
                        Some(i) => i,
                        None => {
                            self.wait_for_receiver_completion();
                            break;
                        }
                    };

                    // Loop until we reached end of file
                    loop {
                        if self.upgrade.bluetooth && !self.upgrade.complete {
                            let mut header = [0u8; 4];
                            match tokio::time::timeout(
                                Duration::from_millis(1),
                                self.socket.read(&mut header[..1]),
                            )
                            .await
                            {
                                Ok(Ok(0)) => {
                                    return Err(anyhow!(
                                        "Receiver closed during Bluetooth transfer"
                                    ));
                                }
                                Ok(Ok(_)) => {
                                    stream_read_exact(&mut self.socket, &mut header[1..]).await?;
                                    Box::pin(self._handle(header)).await?;
                                }
                                Ok(Err(error)) => return Err(error.into()),
                                Err(_) => {}
                            }
                        }
                        // Since this task's runtime is blocked with the outer loop,
                        // OutboundRequest::handle() will not be called again.
                        // Thus, we need to check for cancellation here.
                        match self.receiver.try_recv() {
                            Ok(channel_msg) => {
                                if channel_msg.id == self.state.id {
                                    // TODO: if-let chains will be available in 1.88
                                    if let channel::Message::Lib { action } = &channel_msg.msg {
                                        debug!("outbound: got: {:?}", channel_msg);
                                        match action {
                                            TransferAction::TransferCancel => {
                                                self.update_state(
                                                    |e| {
                                                        e.state = TransferState::Cancelled;
                                                    },
                                                    true,
                                                )
                                                .await;
                                                self.disconnection().await?;
                                                break 'send_all_files;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                match e {
                                    TryRecvError::Empty => {}
                                    e => {
                                        error!("inbound: channel error: {}", e)
                                    }
                                };
                            }
                        };

                        // Workaround to limit scope of the immutable borrow on self
                        let (curr_state, buffer, bytes_read) = {
                            let curr_state = match self.state.transferred_files.get(&current) {
                                Some(s) => s,
                                None => break,
                            };

                            info!("> Currently sending {:?}", curr_state.file_url);
                            if curr_state.bytes_transferred == curr_state.total_size {
                                debug!("File {current} finished");
                                self.update_state(
                                    |e| {
                                        e.transferred_files.remove(&current);
                                    },
                                    false,
                                )
                                .await;
                                break;
                            }

                            if curr_state.file.is_none() {
                                warn!("File {current} is none");
                                break;
                            }

                            let chunk_size = if self.upgrade.bluetooth && !self.upgrade.complete {
                                8 * 1024
                            } else {
                                512 * 1024
                            };
                            let mut buffer = vec![0u8; chunk_size];
                            let bytes_read = curr_state.file.as_ref().unwrap().read(&mut buffer)?;

                            (
                                InternalFileInfo {
                                    payload_id: curr_state.payload_id,
                                    file_url: curr_state.file_url.clone(),
                                    bytes_transferred: curr_state.bytes_transferred,
                                    total_size: curr_state.total_size,
                                    file: None,
                                },
                                buffer,
                                bytes_read,
                            )
                        };

                        let sending_buffer = buffer[..bytes_read].to_vec();
                        info!(
                            "> File ready: {bytes_read} bytes && {} && left to send: {} with current offset: {}",
                            sending_buffer.len(),
                            curr_state.total_size - curr_state.bytes_transferred,
                            curr_state.bytes_transferred
                        );

                        let payload_header = PayloadHeader {
                            id: Some(current),
                            r#type: Some(payload_header::PayloadType::File.into()),
                            total_size: Some(curr_state.total_size),
                            is_sensitive: Some(false),
                            file_name: curr_state
                                .file_url
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned()),
                            ..Default::default()
                        };

                        let wrapper = location_nearby_connections::OfflineFrame {
							version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
							v1: Some(location_nearby_connections::V1Frame {
								r#type: Some(
									location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
								),
								payload_transfer: Some(PayloadTransferFrame {
									packet_type: Some(PacketType::Data.into()),
									payload_chunk: Some(PayloadChunk {
										offset: Some(curr_state.bytes_transferred),
										flags: Some(0),
										body: Some(buffer[..bytes_read].to_vec()),
									}),
									payload_header: Some(payload_header.clone()),
									..Default::default()
								}),
								..Default::default()
							}),
						};

                        self.encrypt_and_send(&wrapper).await?;
                        self.update_state(
                            |e| {
                                if let Some(mu) = e.transferred_files.get_mut(&current) {
                                    mu.bytes_transferred += bytes_read as i64;
                                }

                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                    tmd.ack_bytes += bytes_read as u64;
                                }
                            },
                            true,
                        )
                        .await;

                        // If we just sent the last bytes of the file, mark it as finished
                        if curr_state.bytes_transferred + bytes_read as i64 == curr_state.total_size
                        {
                            debug!(
                                "File {current} finished, curr offset: {} over total: {}",
                                curr_state.bytes_transferred + bytes_read as i64,
                                curr_state.total_size
                            );

                            let wrapper = location_nearby_connections::OfflineFrame {
								version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
								v1: Some(location_nearby_connections::V1Frame {
									r#type: Some(
										location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
									),
									payload_transfer: Some(PayloadTransferFrame {
										packet_type: Some(PacketType::Data.into()),
										payload_chunk: Some(PayloadChunk {
											offset: Some(curr_state.total_size),
											flags: Some(1), // lastChunk
											body: Some(vec![]),
										}),
										payload_header: Some(payload_header),
										..Default::default()
									}),
									..Default::default()
								}),
							};

                            self.encrypt_and_send(&wrapper).await?;
                            break;
                        }
                    }
                }
            }
            sharing_nearby::connection_response_frame::Status::Reject
            | sharing_nearby::connection_response_frame::Status::NotEnoughSpace
            | sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType
            | sharing_nearby::connection_response_frame::Status::TimedOut => {
                warn!(
                    "Cannot process: consent denied: {:?}",
                    v1_frame.connection_response.as_ref().unwrap().status()
                );
                self.update_state(
                    |e| {
                        e.state = TransferState::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
            sharing_nearby::connection_response_frame::Status::Unknown => {
                error!("Unknown consent type: aborting");
                self.update_state(
                    |e| {
                        e.state = TransferState::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
        }

        Ok(())
    }

    fn wait_for_receiver_completion(&mut self) {
        // Google Nearby DelayComplete leaves finalization and disconnect to the receiver.
        info!("All payload bytes sent; waiting for receiver to finish");
        self.completion_deadline = Some(Instant::now() + Duration::from_secs(30));
    }

    async fn receiver_completed(&mut self) -> Result<(), anyhow::Error> {
        self.completion_deadline = None;
        self.update_state(|state| state.state = TransferState::Finished, true)
            .await;
        Err(anyhow!(crate::errors::AppError::NotAnError))
    }

    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame {
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    async fn finalize_key_exchange(
        &mut self,
        raw_peer_key: GenericPublicKey,
    ) -> Result<(), anyhow::Error> {
        let peer_p256_key = raw_peer_key
            .ec_p256_public_key
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        let mut bytes = vec![0x04];
        // Ensure no more than 32 bytes for the keys
        if peer_p256_key.x.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.x[peer_p256_key.x.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.x);
        }
        if peer_p256_key.y.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.y[peer_p256_key.y.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.y);
        }

        let encoded_point = EncodedPoint::from_bytes(bytes)?;
        let peer_key = PublicKey::from_encoded_point(&encoded_point).unwrap();
        let priv_key = self.state.private_key.as_ref().unwrap();

        let dhs = diffie_hellman(priv_key.to_nonzero_scalar(), peer_key.as_affine());
        let derived_secret = Sha256::digest(dhs.raw_secret_bytes());

        let mut ukey_info: Vec<u8> = vec![];
        ukey_info.extend_from_slice(self.state.client_init_msg_data.as_ref().unwrap());
        ukey_info.extend_from_slice(self.state.server_init_data.as_ref().unwrap());

        let auth_label = "UKEY2 v1 auth".as_bytes();
        let next_label = "UKEY2 v1 next".as_bytes();

        let auth_string = hkdf_extract_expand(auth_label, &derived_secret, &ukey_info, 32)?;
        let next_secret = hkdf_extract_expand(next_label, &derived_secret, &ukey_info, 32)?;

        let salt_hex = "82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510";
        let salt =
            hex::decode(salt_hex).map_err(|e| anyhow!("Failed to decode salt_hex: {}", e))?;

        let d2d_client = hkdf_extract_expand(&salt, &next_secret, "client".as_bytes(), 32)?;
        let d2d_server = hkdf_extract_expand(&salt, &next_secret, "server".as_bytes(), 32)?;

        let key_salt_hex = "BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E";
        let key_salt = hex::decode(key_salt_hex)
            .map_err(|e| anyhow!("Failed to decode key_salt_hex: {}", e))?;

        let client_key = hkdf_extract_expand(&key_salt, &d2d_client, "ENC:2".as_bytes(), 32)?;
        let client_hmac_key = hkdf_extract_expand(&key_salt, &d2d_client, "SIG:1".as_bytes(), 32)?;
        let server_key = hkdf_extract_expand(&key_salt, &d2d_server, "ENC:2".as_bytes(), 32)?;
        let server_hmac_key = hkdf_extract_expand(&key_salt, &d2d_server, "SIG:1".as_bytes(), 32)?;

        self.update_state(
            |e| {
                e.decrypt_key = Some(server_key);
                e.recv_hmac_key = Some(server_hmac_key);
                e.encrypt_key = Some(client_key);
                e.send_hmac_key = Some(client_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;

                if let Some(ref mut tm) = e.transfer_metadata {
                    tm.pin_code = Some(to_four_digit_string(&auth_string));
                }
            },
            true,
        )
        .await;

        info!("Pin code: {:?}", self.state.pin_code);

        Ok(())
    }

    async fn send_ukey2_alert(&mut self, atype: AlertType) -> Result<(), anyhow::Error> {
        let alert = Ukey2Alert {
            r#type: Some(atype.into()),
            error_message: None,
        };

        let data = Ukey2Message {
            message_type: Some(atype.into()),
            message_data: Some(alert.encode_to_vec()),
        };

        self.send_frame(data.encode_to_vec()).await
    }

    async fn send_encrypted_frame(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let frame_data = frame.encode_to_vec();
        let body_size = frame_data.len();

        let payload_header = PayloadHeader {
            id: Some(rand::rng().random_range(i64::MIN..i64::MAX)),
            r#type: Some(payload_header::PayloadType::Bytes.into()),
            total_size: Some(body_size as i64),
            is_sensitive: Some(false),
            ..Default::default()
        };

        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(0),
                flags: Some(0),
                body: Some(frame_data),
            }),
            payload_header: Some(payload_header.clone()),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        // Send lastChunk
        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(body_size as i64),
                flags: Some(1), // lastChunk
                body: Some(vec![]),
            }),
            payload_header: Some(payload_header),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        Ok(())
    }

    async fn encrypt_and_send(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        if self.upgrade.last_sent
            && !self.upgrade.complete
            && frame.v1.as_ref().is_some_and(|v| {
                v.r#type()
                    != location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
            })
        {
            if self.upgrade.deferred.len() >= 32 {
                return Err(anyhow!("Too many writes during Wi-Fi upgrade"));
            }
            self.upgrade.deferred.push(frame.clone());
            return Ok(());
        }

        let d2d_msg = DeviceToDeviceMessage {
            sequence_number: Some(self.get_server_seq_inc().await),
            message: Some(frame.encode_to_vec()),
        };

        let key = self.state.encrypt_key.as_ref().unwrap();
        let msg_data = d2d_msg.encode_to_vec();
        let iv = gen_random(16);

        let mut cipher = Cipher::new_256(&key[..AES_256_KEY_LEN].try_into().unwrap());
        cipher.set_auto_padding(true);
        let encrypted = cipher.cbc_encrypt(&iv, &msg_data);

        let hb = HeaderAndBody {
            body: encrypted,
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };

        let mut hmac = HmacSha256::new_from_slice(self.state.send_hmac_key.as_ref().unwrap())?;
        hmac.update(&hb.encode_to_vec());
        let result = hmac.finalize();

        let smsg = SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature: result.into_bytes().to_vec(),
        };

        self.send_frame(smsg.encode_to_vec()).await?;

        Ok(())
    }

    async fn send_keepalive(&mut self, ack: bool) -> Result<(), anyhow::Error> {
        let ack_frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(ack) }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&ack_frame).await
        } else {
            self.send_frame(ack_frame.encode_to_vec()).await
        }
    }

    async fn send_frame(&mut self, data: Vec<u8>) -> Result<(), anyhow::Error> {
        let length = data.len();

        // Prepare length prefix in big-endian format
        let length_bytes = [
            (length >> 24) as u8,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ];

        let mut prefixed_length = Vec::with_capacity(length + 4);
        prefixed_length.extend_from_slice(&length_bytes);
        prefixed_length.extend_from_slice(&data);

        self.socket.write_all(&prefixed_length).await?;
        self.socket.flush().await?;

        Ok(())
    }

    async fn get_server_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.server_seq += 1;
            },
            false,
        )
        .await;

        self.state.server_seq
    }

    async fn get_client_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.client_seq += 1;
            },
            false,
        )
        .await;

        self.state.client_seq
    }

    async fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);
        if let Some(tmd) = self.state.transfer_metadata.as_mut() {
            tmd.transport_medium = Some(self.upgrade.medium_label().to_owned());
        }

        if !inform {
            return;
        }

        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            msg: channel::Message::Client(MessageClient {
                error: None,
                kind: TransferKind::Outbound,
                state: Some(self.state.state.clone()),
                metadata: self.state.transfer_metadata.clone(),
            }),
        });
        // Add a small sleep timer to allow the Tokio runtime to have
        // some spare time to process channel's message. Otherwise it
        // get spammed by new requests. Currently set to 10 micro secs.
        tokio::time::sleep(SANITY_DURATION).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    async fn text_sender() -> (OutboundRequest, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (events, _) = broadcast::channel(50);
        let mut request = OutboundRequest::new(
            *b"TEST",
            socket,
            "test-peer".into(),
            events,
            OutboundPayload::Text {
                text: "test".into(),
                text_type: TextPayloadType::Text,
            },
            RemoteDeviceInfo {
                name: "test-peer".into(),
                device_type: DeviceType::Laptop,
            },
        );
        request.state.encrypt_key = Some(vec![1; 32]);
        request.state.send_hmac_key = Some(vec![2; 32]);
        request.state.text_payload = Some(TextPayloadInfo::Text(42));
        (request, peer)
    }

    fn consent() -> sharing_nearby::V1Frame {
        sharing_nearby::V1Frame {
            r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
            connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                status: Some(sharing_nearby::connection_response_frame::Status::Accept.into()),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn sent_bytes_are_not_reported_as_receiver_completion() {
        let (mut request, _peer) = text_sender().await;
        request.process_consent(&consent()).await.unwrap();
        assert_eq!(request.state.state, TransferState::SendingFiles);
    }

    #[tokio::test]
    async fn handover_freezes_crypto_sequence_and_does_not_send_file_over_bluetooth() {
        let (mut request, mut peer) = text_sender().await;
        request.upgrade.bluetooth = true;
        request.upgrade.last_sent = true;
        let sequence = request.state.server_seq;
        request.send_keepalive(true).await.unwrap();
        assert_eq!(request.state.server_seq, sequence);
        assert_eq!(request.upgrade.deferred.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        request.process_consent(&consent()).await.unwrap();
        assert!(request.deferred_consent.is_some());
        assert_eq!(request.state.server_seq, sequence);
        request.upgrade.complete = true;
        for frame in std::mem::take(&mut request.upgrade.deferred) {
            request.encrypt_and_send(&frame).await.unwrap();
        }
        assert_eq!(request.state.server_seq, sequence + 1);
        assert!(peer.read_u32().await.unwrap() > 0);
    }

    async fn drain_payload(peer: &mut TcpStream) {
        for _ in 0..2 {
            let len = peer.read_u32().await.unwrap();
            assert!(len < SANE_FRAME_LENGTH as u32);
            let mut bytes = vec![0; len as usize];
            peer.read_exact(&mut bytes).await.unwrap();
        }
    }

    async fn next_error(request: &mut OutboundRequest) -> anyhow::Error {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Err(error) = request.handle().await {
                    return error;
                }
            }
        })
        .await
        .expect("request must terminate")
    }

    #[tokio::test]
    async fn receiver_closure_completes_without_a_sender_disconnect() {
        let (mut request, mut peer) = text_sender().await;
        request.process_consent(&consent()).await.unwrap();
        drain_payload(&mut peer).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        peer.shutdown().await.unwrap();
        let result = next_error(&mut request).await;
        assert!(result.downcast_ref::<crate::errors::AppError>().is_some());
        assert_eq!(request.state.state, TransferState::Finished);
    }

    #[tokio::test]
    async fn file_transfer_waits_for_receiver_and_timeout_is_not_success() {
        let (mut request, mut peer) = text_sender().await;
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let file = File::open(&path).unwrap();
        request.payload = OutboundPayload::Files(vec![path.to_string_lossy().into_owned()]);
        request.state.text_payload = None;
        request.state.transferred_files.insert(
            7,
            InternalFileInfo {
                payload_id: 7,
                total_size: file.metadata().unwrap().len() as i64,
                bytes_transferred: 0,
                file_url: path,
                file: Some(file),
            },
        );
        request.process_consent(&consent()).await.unwrap();
        drain_payload(&mut peer).await;
        assert_eq!(request.state.state, TransferState::SendingFiles);
        request.completion_deadline = Some(Instant::now());
        assert!(
            next_error(&mut request)
                .await
                .to_string()
                .contains("within 30 seconds")
        );
        assert_ne!(request.state.state, TransferState::Finished);
    }

    fn control_frame(id: i64, event: i32) -> OfflineFrame {
        OfflineFrame {
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::Control.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(id),
                        ..Default::default()
                    }),
                    control_message: Some(
                        location_nearby_connections::payload_transfer_frame::ControlMessage {
                            event: Some(event),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn receiver_control_messages_are_not_mistaken_for_data_chunks() {
        use crate::location_nearby_connections::payload_transfer_frame::control_message::EventType;
        let (mut request, _peer) = text_sender().await;
        request.process_consent(&consent()).await.unwrap();
        request
            .process_offline_frame(&control_frame(42, EventType::PayloadReceivedAck.into()))
            .await
            .unwrap();
        assert_eq!(request.state.state, TransferState::SendingFiles);
        request
            .process_offline_frame(&control_frame(999, EventType::PayloadError.into()))
            .await
            .unwrap();
        for event in [EventType::PayloadError, EventType::PayloadCanceled] {
            assert!(
                request
                    .process_offline_frame(&control_frame(42, event.into()))
                    .await
                    .is_err()
            );
            assert_ne!(request.state.state, TransferState::Finished);
        }
    }

    #[tokio::test]
    async fn incomplete_frame_after_payload_is_not_success() {
        let (mut request, mut peer) = text_sender().await;
        request.process_consent(&consent()).await.unwrap();
        drain_payload(&mut peer).await;
        peer.write_all(&[0, 0]).await.unwrap();
        peer.shutdown().await.unwrap();
        assert!(
            next_error(&mut request)
                .await
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        assert_ne!(request.state.state, TransferState::Finished);
    }

    #[tokio::test]
    async fn receiver_disconnect_frame_finishes_only_after_payload() {
        let frame = OfflineFrame {
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (mut request, _peer) = text_sender().await;
        assert!(request.process_offline_frame(&frame).await.is_err());
        assert_ne!(request.state.state, TransferState::Finished);
        request.process_consent(&consent()).await.unwrap();
        assert!(request.process_offline_frame(&frame).await.is_err());
        assert_eq!(request.state.state, TransferState::Finished);
    }

    #[tokio::test]
    async fn stalled_partial_frame_does_not_bypass_completion_deadline() {
        let (mut request, mut peer) = text_sender().await;
        request.process_consent(&consent()).await.unwrap();
        drain_payload(&mut peer).await;
        peer.write_all(&[0, 0]).await.unwrap();
        request.completion_deadline = Some(Instant::now() + Duration::from_millis(30));
        assert!(
            next_error(&mut request)
                .await
                .to_string()
                .contains("within 30 seconds")
        );
        assert_ne!(request.state.state, TransferState::Finished);
    }

    #[tokio::test]
    async fn failed_lan_releases_large_payload_consent_on_bluetooth() {
        let (mut request, mut peer) = text_sender().await;
        request.upgrade.bluetooth = true;
        request
            .state
            .transfer_metadata
            .as_mut()
            .unwrap()
            .total_bytes = 16 * 1024 * 1024;
        request.process_consent(&consent()).await.unwrap();
        assert!(request.deferred_consent.is_some());
        request.upgrade.lan_failed = true;
        request.handle().await.unwrap();
        drain_payload(&mut peer).await;
        assert_eq!(request.state.state, TransferState::SendingFiles);
        assert!(request.upgrade.deadline.is_none());
        assert!(!request.upgrade.complete);
    }

    #[tokio::test]
    async fn small_bluetooth_payload_does_not_wait_for_an_upgrade() {
        let (mut request, mut peer) = text_sender().await;
        request.upgrade.bluetooth = true;
        request.upgrade.deadline = Some(Instant::now() + Duration::from_secs(60));
        request.process_consent(&consent()).await.unwrap();
        assert!(request.deferred_consent.is_none());
        assert!(request.upgrade.deadline.is_none());
        drain_payload(&mut peer).await;
        assert_eq!(request.state.state, TransferState::SendingFiles);
    }

    #[tokio::test]
    async fn stalled_partial_frame_does_not_bypass_upgrade_deadline() {
        let (mut request, mut peer) = text_sender().await;
        request.upgrade.bluetooth = true;
        request.upgrade.deadline = Some(Instant::now() + Duration::from_millis(30));
        peer.write_all(&[0, 0]).await.unwrap();
        assert!(
            next_error(&mut request)
                .await
                .to_string()
                .contains("Wi-Fi handover timed out")
        );
    }

    #[tokio::test]
    async fn encrypted_file_introduction_preserves_attachment_payload_mapping() {
        let (mut sender, peer) = text_sender().await;
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        sender.payload = OutboundPayload::Files(vec![
            root.join("Cargo.toml").to_string_lossy().into_owned(),
            root.join("LICENSE").to_string_lossy().into_owned(),
        ]);
        sender
            .process_paired_key_result(&sharing_nearby::V1Frame {
                paired_key_result: Some(sharing_nearby::PairedKeyResultFrame {
                    status: Some(paired_key_result_frame::Status::Unable.into()),
                }),
                ..Default::default()
            })
            .await
            .unwrap();

        let (events, _) = broadcast::channel(50);
        let mut receiver = OutboundRequest::new(
            *b"PEER",
            peer,
            "test-sender".into(),
            events,
            OutboundPayload::Files(vec![]),
            RemoteDeviceInfo {
                name: "test-sender".into(),
                device_type: DeviceType::Laptop,
            },
        );
        receiver.state.decrypt_key = Some(vec![1; 32]);
        receiver.state.recv_hmac_key = Some(vec![2; 32]);
        receiver.state.state = TransferState::SendingFiles;
        for _ in 0..2 {
            let length = receiver.socket.read_u32().await.unwrap();
            receiver._handle(length.to_be_bytes()).await.unwrap();
        }
        assert_eq!(receiver.state.payload_buffers.len(), 1);
        let bytes = receiver.state.payload_buffers.values().next().unwrap();
        let frame = sharing_nearby::Frame::decode(bytes.as_slice()).unwrap();
        let files = frame.v1.unwrap().introduction.unwrap().file_metadata;
        assert_eq!(files.len(), 2);
        let mut mapping = HashMap::new();
        for file in files {
            assert_ne!(
                file.id(),
                0,
                "Google replaces zero attachment IDs, breaking payload lookup"
            );
            assert!(mapping.insert(file.id(), file.payload_id()).is_none());
            assert!(
                sender
                    .state
                    .transferred_files
                    .contains_key(&file.payload_id())
            );
        }
    }

    #[tokio::test]
    async fn connection_request_advertises_p2p_and_hotspot_mediums() {
        let (mut sender, mut peer) = text_sender().await;
        sender.send_connection_request().await.unwrap();
        let len = peer.read_u32().await.unwrap();
        let mut buf = vec![0u8; len as usize];
        peer.read_exact(&mut buf).await.unwrap();
        let offline = OfflineFrame::decode(buf.as_slice()).unwrap();
        let conn_req = offline
            .v1
            .unwrap()
            .connection_request
            .unwrap();
        let mediums = conn_req.mediums;
        assert!(mediums.contains(&(Medium::WifiLan as i32)));
        assert!(mediums.contains(&(Medium::WifiDirect as i32)));
        assert!(mediums.contains(&(Medium::WifiHotspot as i32)));
    }

    #[tokio::test]
    async fn outbound_bandwidth_upgrade_retry_request_replies_supported_mediums() {
        let (mut sender, mut peer) = text_sender().await;
        sender.state.encrypt_key = Some(vec![1; 32]);
        sender.state.send_hmac_key = Some(vec![2; 32]);
        sender.upgrade.bluetooth = true;

        let retry = location_nearby_connections::BandwidthUpgradeRetryFrame {
            is_request: Some(true),
            supported_medium: vec![
                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiHotspot
                    .into(),
                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiDirect
                    .into(),
                location_nearby_connections::bandwidth_upgrade_retry_frame::Medium::WifiLan.into(),
            ],
        };

        sender
            .process_bandwidth_upgrade_retry(&retry)
            .await
            .unwrap();
        assert!(peer.read_u32().await.unwrap() > 0);
    }

    #[tokio::test]
    async fn sender_proactively_offers_wifi_direct_on_consent_when_bluetooth() {
        let (mut sender, mut peer) = text_sender().await;
        sender.state.encrypt_key = Some(vec![1; 32]);
        sender.state.send_hmac_key = Some(vec![2; 32]);
        sender.upgrade.bluetooth = true;
        let mut ctx = crate::bwu::Context::new(*b"LOCL", 9300);
        ctx.address = None;
        ctx.wifi_direct = Some(crate::bwu::WifiDirectOffer {
            ssid: "DIRECT-test-sender".into(),
            password: "testpassword123".into(),
            gateway: std::net::Ipv4Addr::new(192, 168, 49, 1),
            port: 9300,
            frequency: 5180,
        });
        sender.upgrade.context = Some(ctx);
        sender.state.transfer_metadata.as_mut().unwrap().total_bytes = 10 * 1024 * 1024;

        sender.process_consent(&consent()).await.unwrap();
        assert!(sender.deferred_consent.is_some());
        assert!(sender.upgrade.direct_offered.is_some());
        // Verify peer received encrypted offer frame
        assert!(peer.read_u32().await.unwrap() > 0);
    }
}
