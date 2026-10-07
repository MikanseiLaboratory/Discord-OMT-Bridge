use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use crate::dave::{client_binary, split_server_binary, DaveState, DAVE_VERSION};
use crate::decode::{H264Decoder, OpusDecoder};
use crate::jitter::JitterBuffer;
use crate::sdp::{advertisements_from_offer, payload_of, select_protocol_fragment, synthesize_answer};
use crate::MediaError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Voice,
    GoLive,
}

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub endpoint: String,
    pub token: String,
    pub session_id: String,
    pub server_id: u64,
    pub user_id: u64,
    pub channel_id: u64,
    pub kind: MediaKind,
    pub max_video_slots: usize,
    pub prefer_quality: u32,
}

#[derive(Debug, Clone)]
pub enum MediaEvent {
    Audio {
        user_id: u64,
        pcm: Vec<i16>,
        sample_rate: u32,
        channels: u16,
    },
    Video {
        user_id: u64,
        bgra: Vec<u8>,
        width: u32,
        height: u32,
    },
    VideoActive {
        user_id: u64,
        active: bool,
    },
    Disconnected {
        reason: String,
    },
}

pub struct MediaHandle {
    pub events: mpsc::Receiver<MediaEvent>,
    pub cancel: CancellationToken,
}

struct RtpPacket {
    payload: Vec<u8>,
    marker: bool,
    payload_type: u8,
}

struct SsrcBook {
    audio: HashMap<u32, u64>,
    video: HashMap<u32, u64>,
    rtx: HashSet<u32>,
    users: Vec<u64>,
}

impl SsrcBook {
    fn new() -> Self {
        Self {
            audio: HashMap::new(),
            video: HashMap::new(),
            rtx: HashSet::new(),
            users: Vec::new(),
        }
    }

    fn remember_user(&mut self, user_id: u64) {
        if user_id != 0 && !self.users.contains(&user_id) {
            self.users.push(user_id);
        }
    }
}

pub fn spawn_media(params: ConnectParams) -> MediaHandle {
    let cancel = CancellationToken::new();
    let (tx, events) = mpsc::channel(128);
    let child_cancel = cancel.clone();
    tokio::spawn(async move {
        if let Err(err) = run_connection(params, tx.clone(), child_cancel).await {
            tracing::warn!(%err, "media connection ended");
            let _ = tx
                .send(MediaEvent::Disconnected {
                    reason: err.to_string(),
                })
                .await;
        }
    });
    MediaHandle { events, cancel }
}

async fn run_connection(
    params: ConnectParams,
    events: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let endpoint = params.endpoint.trim_end_matches('/').trim_start_matches("wss://");
    let url = format!("wss://{endpoint}/?v=8");
    tracing::info!(url, ?params.kind, "connecting voice gateway");
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|err| MediaError::Voice(err.to_string()))?;

    let (rtp_tx, mut rtp_rx) = mpsc::channel::<(RTPCodecType, webrtc::rtp::packet::Packet)>(256);
    let peer = prepare_peer(params.max_video_slots, rtp_tx).await?;

    let mut dave = DaveState::new(params.user_id, params.channel_id)?;
    let mut book = SsrcBook::new();
    let mut heartbeat = interval(Duration::from_secs(4));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heartbeats_started = false;
    let mut opus = OpusDecoder::new()?;
    let mut h264 = H264Decoder::new()?;
    let mut audio_jitter: HashMap<u32, JitterBuffer<RtpPacket>> = HashMap::new();
    let mut video_jitter: HashMap<u32, JitterBuffer<RtpPacket>> = HashMap::new();
    let mut assemblers: HashMap<u32, H264Packet> = HashMap::new();
    let mut access_units: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut h264_payload = 102u8;
    let mut rtx_payloads = HashSet::new();
    let mut remote_video_ssrcs: Vec<u32> = Vec::new();
    let mut local_sdp = String::new();
    let mut answered = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = heartbeat.tick(), if heartbeats_started => {
                let nonce = json!({"op": 3, "d": { "t": chrono_millis() }});
                socket.send(Message::Text(nonce.to_string().into())).await?;
            }
            incoming = socket.next() => {
                let Some(message) = incoming else { break };
                let message = message?;
                match message {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text)?;
                        let op = payload.get("op").and_then(|value| value.as_u64()).unwrap_or(99);
                        match op {
                            8 => {
                                if let Some(ms) = payload.pointer("/d/heartbeat_interval").and_then(|v| v.as_u64()) {
                                    heartbeat = interval(Duration::from_millis(ms));
                                    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                                }
                                heartbeats_started = true;
                                let identify = json!({
                                    "op": 0,
                                    "d": {
                                        "server_id": params.server_id.to_string(),
                                        "user_id": params.user_id.to_string(),
                                        "session_id": params.session_id,
                                        "token": params.token,
                                        "video": true,
                                        "max_dave_protocol_version": DAVE_VERSION
                                    }
                                });
                                socket.send(Message::Text(identify.to_string().into())).await?;
                            }
                            2 => {
                                let (fragment, codecs, sdp, h264_pt, rtx) = offer_bundle(&peer).await?;
                                local_sdp = sdp;
                                h264_payload = h264_pt;
                                rtx_payloads = rtx;
                                let select = json!({
                                    "op": 1,
                                    "d": {
                                        "protocol": "webrtc",
                                        "data": fragment,
                                        "codecs": codecs,
                                        "rtc_connection_id": uuid_like(params.user_id)
                                    }
                                });
                                socket.send(Message::Text(select.to_string().into())).await?;
                            }
                            4 => {
                                let data = &payload["d"];
                                let version = data.get("dave_protocol_version").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                                dave.set_protocol_version(version);
                                if version > 0 {
                                    let package = dave.key_package()?;
                                    socket.send(Message::Binary(client_binary(26, &package).into())).await?;
                                } else {
                                    dave.enable_passthrough();
                                }
                                if let Some(sdp) = data.get("sdp").and_then(|v| v.as_str()) {
                                    apply_answer(&peer, &local_sdp, sdp, &remote_video_ssrcs).await?;
                                    answered = true;
                                }
                                let wants = json!({"op": 15, "d": {"any": params.prefer_quality.min(100)}});
                                socket.send(Message::Text(wants.to_string().into())).await?;
                            }
                            5 => {
                                let user_id = parse_id(payload.pointer("/d/user_id")).unwrap_or(0);
                                let ssrc = payload.pointer("/d/ssrc").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                if user_id != 0 && ssrc != 0 {
                                    book.audio.insert(ssrc, user_id);
                                    book.remember_user(user_id);
                                }
                            }
                            12 => {
                                handle_video_opcode(&payload["d"], &mut book, &mut remote_video_ssrcs, &events, params.prefer_quality).await;
                                if answered {
                                    if let Some(sdp) = payload.pointer("/d/sdp").and_then(|v| v.as_str()) {
                                        apply_answer(&peer, &local_sdp, sdp, &remote_video_ssrcs).await?;
                                    }
                                }
                                let quality = params.prefer_quality.min(100);
                                let wants = json!({"op": 15, "d": {"any": quality}});
                                socket.send(Message::Text(wants.to_string().into())).await?;
                            }
                            21 => {
                                let transition = payload.pointer("/d/transition_id").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                                let version = payload.pointer("/d/protocol_version").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                                dave.note_transition(transition);
                                if version == 0 {
                                    dave.enable_passthrough();
                                }
                                if transition == 0 || version == 0 {
                                    let ready = json!({"op": 23, "d": {"transition_id": transition}});
                                    socket.send(Message::Text(ready.to_string().into())).await?;
                                }
                            }
                            22 => {
                                let version = dave.protocol_version();
                                if version == 0 {
                                    dave.enable_passthrough();
                                }
                            }
                            24 => {
                                let epoch = payload.pointer("/d/epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                                let version = payload.pointer("/d/protocol_version").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                                dave.set_protocol_version(version);
                                if epoch == 1 {
                                    dave.rekey()?;
                                    let package = dave.key_package()?;
                                    socket.send(Message::Binary(client_binary(26, &package).into())).await?;
                                }
                            }
                            31 => {
                                tracing::warn!("voice gateway reported an invalid MLS commit");
                            }
                            _ => {}
                        }
                    }
                    Message::Binary(bytes) => {
                        if let Some((opcode, payload)) = split_server_binary(&bytes) {
                            handle_dave_binary(opcode, payload, &mut dave, &book, &mut socket).await?;
                        }
                    }
                    Message::Close(frame) => {
                        return Err(MediaError::Voice(format!("voice closed: {frame:?}")));
                    }
                    _ => {}
                }
            }
            packet = rtp_rx.recv() => {
                let Some((kind, packet)) = packet else { break };
                let header = &packet.header;
                let item = RtpPacket {
                    payload: packet.payload.to_vec(),
                    marker: header.marker,
                    payload_type: header.payload_type,
                };
                if rtx_payloads.contains(&item.payload_type) || book.rtx.contains(&header.ssrc) {
                    continue;
                }
                let buffer = if kind == RTPCodecType::Audio {
                    audio_jitter.entry(header.ssrc).or_insert_with(|| JitterBuffer::new(32))
                } else {
                    video_jitter.entry(header.ssrc).or_insert_with(|| JitterBuffer::new(64))
                };
                for (_seq, ready) in buffer.push(header.sequence_number, item) {
                    dispatch_rtp(
                        kind,
                        header.ssrc,
                        ready,
                        &book,
                        &mut dave,
                        &mut opus,
                        &mut h264,
                        &mut assemblers,
                        &mut access_units,
                        h264_payload,
                        &events,
                    ).await;
                }
            }
        }
    }
    Err(MediaError::Voice("voice socket ended".into()))
}

async fn dispatch_rtp(
    kind: RTPCodecType,
    ssrc: u32,
    packet: RtpPacket,
    book: &SsrcBook,
    dave: &mut DaveState,
    opus: &mut OpusDecoder,
    h264: &mut H264Decoder,
    assemblers: &mut HashMap<u32, H264Packet>,
    access_units: &mut HashMap<u32, Vec<u8>>,
    h264_payload: u8,
    events: &mpsc::Sender<MediaEvent>,
) {
    let Some(user_id) = (if kind == RTPCodecType::Audio {
        book.audio.get(&ssrc).copied()
    } else {
        book.video.get(&ssrc).copied().or_else(|| book.audio.get(&ssrc).copied())
    }) else {
        return;
    };
    if kind == RTPCodecType::Audio {
        let plain = match dave.decrypt(user_id, false, &packet.payload) {
            Ok(plain) => plain,
            Err(err) => {
                tracing::debug!(%err, user_id, "audio decrypt failed");
                return;
            }
        };
        match opus.decode(&plain) {
            Ok(pcm) if !pcm.is_empty() => {
                let _ = events
                    .send(MediaEvent::Audio {
                        user_id,
                        pcm,
                        sample_rate: 48_000,
                        channels: 2,
                    })
                    .await;
            }
            Ok(_) => {}
            Err(err) => tracing::debug!(%err, user_id, "opus decode failed"),
        }
        return;
    }
    if packet.payload_type != h264_payload && h264_payload != 0 {
        tracing::debug!(payload = packet.payload_type, "dropping non-H264 video");
        return;
    }
    let depacketizer = assemblers.entry(ssrc).or_default();
    let piece = match depacketizer.depacketize(&Bytes::from(packet.payload)) {
        Ok(piece) => piece,
        Err(err) => {
            tracing::debug!(%err, "h264 depacketize failed");
            return;
        }
    };
    let unit = access_units.entry(ssrc).or_default();
    unit.extend_from_slice(&piece);
    if !packet.marker && unit.len() < 512 * 1024 {
        return;
    }
    let access_unit = std::mem::take(unit);
    let plain = match dave.decrypt(user_id, true, &access_unit) {
        Ok(plain) => plain,
        Err(err) => {
            tracing::debug!(%err, user_id, "video decrypt failed");
            return;
        }
    };
    match h264.decode(&plain) {
        Ok(Some((bgra, width, height))) => {
            let _ = events
                .send(MediaEvent::Video {
                    user_id,
                    bgra,
                    width,
                    height,
                })
                .await;
        }
        Ok(None) => {}
        Err(err) => tracing::debug!(%err, user_id, "h264 decode failed"),
    }
}

async fn handle_video_opcode(
    data: &Value,
    book: &mut SsrcBook,
    remote_video_ssrcs: &mut Vec<u32>,
    events: &mpsc::Sender<MediaEvent>,
    prefer_quality: u32,
) {
    let user_id = parse_id(data.get("user_id")).unwrap_or(0);
    if user_id == 0 {
        return;
    }
    book.remember_user(user_id);
    if let Some(audio) = data.get("audio_ssrc").and_then(|value| value.as_u64()) {
        if audio > 0 {
            book.audio.insert(audio as u32, user_id);
        }
    }
    let video = data.get("video_ssrc").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
    let active_streams = data
        .get("streams")
        .and_then(|value| value.as_array())
        .map(|streams| {
            streams
                .iter()
                .filter(|stream| stream.get("active").and_then(|value| value.as_bool()).unwrap_or(false))
                .filter_map(|stream| {
                    let ssrc = stream.get("ssrc").and_then(|value| value.as_u64())? as u32;
                    let quality = stream.get("quality").and_then(|value| value.as_u64()).unwrap_or(100) as u32;
                    let rtx = stream.get("rtx_ssrc").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
                    Some((ssrc, quality, rtx))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (_, _, rtx) in &active_streams {
        if *rtx != 0 {
            book.rtx.insert(*rtx);
        }
    }
    let chosen = active_streams
        .iter()
        .filter(|(_, quality, _)| *quality <= prefer_quality.max(1))
        .max_by_key(|(_, quality, _)| *quality)
        .map(|(ssrc, _, _)| *ssrc)
        .or_else(|| active_streams.first().map(|(ssrc, _, _)| *ssrc))
        .unwrap_or(video);
    let camera_on = chosen != 0 && (video != 0 || !active_streams.is_empty());
    if chosen != 0 {
        book.video.insert(chosen, user_id);
        if !remote_video_ssrcs.contains(&chosen) {
            remote_video_ssrcs.push(chosen);
        }
    } else if video == 0 {
        book.video.retain(|_, owner| *owner != user_id);
    }
    let _ = events
        .send(MediaEvent::VideoActive {
            user_id,
            active: camera_on,
        })
        .await;
}

async fn handle_dave_binary(
    opcode: u8,
    payload: &[u8],
    dave: &mut DaveState,
    book: &SsrcBook,
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<(), MediaError> {
    match opcode {
        25 => dave.external_sender(payload)?,
        27 => {
            if payload.is_empty() {
                return Ok(());
            }
            let operation = payload[0];
            if let Some(commit) = dave.proposals(operation, &payload[1..], &book.users)? {
                let mut body = commit.commit;
                if let Some(welcome) = commit.welcome {
                    body.extend_from_slice(&welcome);
                }
                socket
                    .send(Message::Binary(client_binary(28, &body).into()))
                    .await?;
            }
        }
        29 => {
            if payload.len() < 2 {
                return Ok(());
            }
            let transition = u16::from_be_bytes([payload[0], payload[1]]);
            dave.note_transition(transition);
            if let Err(err) = dave.commit(&payload[2..]) {
                tracing::warn!(%err, "MLS commit rejected");
                let invalid = json!({"op": 31, "d": {"transition_id": transition}});
                socket.send(Message::Text(invalid.to_string().into())).await?;
                return Ok(());
            }
            let ready = json!({"op": 23, "d": {"transition_id": transition}});
            socket.send(Message::Text(ready.to_string().into())).await?;
        }
        30 => {
            if payload.len() < 2 {
                return Ok(());
            }
            let transition = u16::from_be_bytes([payload[0], payload[1]]);
            dave.note_transition(transition);
            if let Err(err) = dave.welcome(&payload[2..]) {
                tracing::warn!(%err, "MLS welcome rejected");
                let invalid = json!({"op": 31, "d": {"transition_id": transition}});
                socket.send(Message::Text(invalid.to_string().into())).await?;
                return Ok(());
            }
            let ready = json!({"op": 23, "d": {"transition_id": transition}});
            socket.send(Message::Text(ready.to_string().into())).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn prepare_peer(
    video_slots: usize,
    rtp_tx: mpsc::Sender<(RTPCodecType, webrtc::rtp::packet::Packet)>,
) -> Result<Arc<webrtc::peer_connection::RTCPeerConnection>, MediaError> {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    let peer = api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    let recvonly = || RTCRtpTransceiverInit {
        direction: RTCRtpTransceiverDirection::Recvonly,
        send_encodings: Vec::new(),
    };
    peer.add_transceiver_from_kind(RTPCodecType::Audio, Some(recvonly()))
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    for _ in 0..video_slots.max(1) {
        peer.add_transceiver_from_kind(RTPCodecType::Video, Some(recvonly()))
            .await
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    }
    let rtp_tx = Arc::new(tokio::sync::Mutex::new(rtp_tx));
    peer.on_track(Box::new(move |track, _, _| {
        let rtp_tx = Arc::clone(&rtp_tx);
        let kind = track.kind();
        Box::pin(async move {
            loop {
                match track.read_rtp().await {
                    Ok((packet, _)) => {
                        if rtp_tx.lock().await.try_send((kind, packet)).is_err() {
                            tracing::debug!("rtp queue full");
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    }));
    Ok(Arc::new(peer))
}

async fn offer_bundle(
    peer: &webrtc::peer_connection::RTCPeerConnection,
) -> Result<(String, Value, String, u8, HashSet<u8>), MediaError> {
    let offer = peer
        .create_offer(None)
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    peer.set_local_description(offer)
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    let mut gathering = peer.gathering_complete_promise().await;
    let _ = gathering.recv().await;
    let local = peer
        .local_description()
        .await
        .ok_or_else(|| MediaError::WebRtc("missing local description".into()))?;
    let sdp = local.sdp;
    let vp8_rtx = crate::sdp::payload_of(&sdp, "rtx");
    let fragment = select_protocol_fragment(&sdp, vp8_rtx);
    let codecs = advertisements_from_offer(&sdp);
    let mut rtx = HashSet::new();
    let json_codecs: Vec<Value> = codecs
        .iter()
        .map(|codec| {
            if let Some(rtx_pt) = codec.rtx_payload_type {
                rtx.insert(rtx_pt);
            }
            json!({
                "name": codec.name,
                "type": codec.kind,
                "priority": codec.priority,
                "payload_type": codec.payload_type,
                "rtx_payload_type": codec.rtx_payload_type,
                "encode": codec.encode,
                "decode": codec.decode
            })
        })
        .collect();
    let h264 = payload_of(&sdp, "H264").unwrap_or(102);
    Ok((fragment, Value::Array(json_codecs), sdp, h264, rtx))
}

async fn apply_answer(
    peer: &webrtc::peer_connection::RTCPeerConnection,
    local_sdp: &str,
    server_sdp: &str,
    remote_video_ssrcs: &[u32],
) -> Result<(), MediaError> {
    let answer = synthesize_answer(local_sdp, server_sdp, remote_video_ssrcs)
        .ok_or_else(|| MediaError::WebRtc("server SDP is missing ICE or DTLS".into()))?;
    let description = RTCSessionDescription::answer(answer)
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    peer.set_remote_description(description)
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))
}

fn parse_id(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse().ok())
            .or_else(|| value.as_u64())
    })
}

fn chrono_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn uuid_like(user_id: u64) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (user_id >> 32) as u32,
        (user_id >> 16) as u16,
        (user_id & 0xfff) as u16,
        ((user_id >> 12) & 0xfff) as u16,
        user_id & 0xffffffffffff
    )
}
