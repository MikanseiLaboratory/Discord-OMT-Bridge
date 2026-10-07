use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use crate::dave::{client_binary, split_server_binary, DaveState, DAVE_VERSION};
use crate::decode::{Av1Assembler, Av1Decoder, H264Decoder, OpusDecoder};
use crate::jitter::JitterBuffer;
use crate::sdp::{
    advertisements_from_offer, payload_of, select_protocol_fragment, synthesize_answer, AssignedSsrc,
};
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
}

impl SsrcBook {
    fn new() -> Self {
        Self {
            audio: HashMap::new(),
            video: HashMap::new(),
            rtx: HashSet::new(),
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

    let (rtp_tx, mut rtp_rx) = mpsc::channel::<(RTPCodecType, webrtc::rtp::packet::Packet)>(2048);
    let video_jobs = spawn_video_decoder(events.clone());
    let mut wait_for_keyframe = false;
    let peer = prepare_peer(params.max_video_slots, rtp_tx).await?;
    let (failed_tx, mut failed_rx) = mpsc::channel::<()>(1);
    peer.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let failed_notify = failed_tx.clone();
        Box::pin(async move {
            tracing::info!(?state, "peer connection state");
            if state == RTCPeerConnectionState::Failed {
                let _ = failed_notify.try_send(());
            }
        })
    }));

    // A Go Live stream is its own MLS group. The group id is the media-session
    // id, one less than the stream rtc_server_id, not the parent voice channel.
    let dave_channel = match params.kind {
        MediaKind::GoLive if params.server_id > 0 => params.server_id - 1,
        _ => params.channel_id,
    };
    tracing::info!(?params.kind, dave_channel, "dave group");
    let mut dave = DaveState::new(params.user_id, dave_channel)?;
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
    let mut av1_frames: HashMap<u32, Av1Assembler> = HashMap::new();
    let mut h264_payload = 102u8;
    let mut rtx_payloads = HashSet::new();
    let mut audio_streams: Vec<AssignedSsrc> = Vec::new();
    let mut video_streams: Vec<AssignedSsrc> = Vec::new();
    let mut applied_audio: Vec<u32> = Vec::new();
    let mut applied_video: Vec<u32> = Vec::new();
    let mut local_sdp = String::new();
    let mut server_sdp = String::new();
    let mut answered = false;
    let mut keyframe_requests = 0u32;
    let mut answer_deadline: Option<Instant> = None;
    let mut local_audio_ssrc = 0u32;
    let mut video_announce: Option<String> = None;
    let mut video_rtp = 0u32;
    let mut video_wait: Option<Instant> = None;
    let mut answer_tick = interval(Duration::from_millis(200));
    answer_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = answer_tick.tick() => {
                if !server_sdp.is_empty() && !answered {
                    if answer_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        publish_description(&peer, &mut local_sdp, &server_sdp, &audio_streams, &video_streams, false).await?;
                        answered = true;
                        applied_audio = audio_streams.iter().map(|stream| stream.ssrc).collect();
                        applied_video = video_streams.iter().map(|stream| stream.ssrc).collect();
                    }
                }
                if answered {
                    keyframe_requests = keyframe_requests.saturating_add(1);
                    if keyframe_requests % 5 == 0 {
                        let wants = sink_wants(params.prefer_quality, &video_streams);
                        socket.send(Message::Text(wants.to_string().into())).await?;
                        for stream in &video_streams {
                            if stream.ssrc == 0 {
                                continue;
                            }
                            let pli = PictureLossIndication {
                                sender_ssrc: 1,
                                media_ssrc: stream.ssrc,
                            };
                            let _ = peer.write_rtcp(&[Box::new(pli)]).await;
                        }
                        if keyframe_requests <= 20 {
                            tracing::info!(streams = video_streams.len(), "requested video keyframe");
                        }
                    }
                    if params.kind == MediaKind::GoLive
                        && video_rtp == 0
                        && video_streams.iter().any(|stream| stream.ssrc != 0)
                    {
                        if video_wait.is_none() {
                            video_wait = Some(Instant::now());
                        } else if video_wait.is_some_and(|start| start.elapsed() > Duration::from_secs(4)) {
                            return Err(MediaError::Voice("Go Live video RTP did not arrive".into()));
                        }
                    }
                }
            }
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
                                        "streams": [{ "type": "video", "rid": "100", "quality": 100 }],
                                        "max_dave_protocol_version": DAVE_VERSION
                                    }
                                });
                                socket.send(Message::Text(identify.to_string().into())).await?;
                            }
                            2 => {
                                local_audio_ssrc = payload.pointer("/d/ssrc").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
                                let ready_streams = payload.pointer("/d/streams").cloned().unwrap_or(json!([]));
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
                                if local_audio_ssrc != 0 {
                                    let announce = inactive_video(local_audio_ssrc, &ready_streams);
                                    video_announce = Some(announce.to_string());
                                    socket.send(Message::Text(announce.to_string().into())).await?;
                                    tracing::info!(
                                        audio_ssrc = local_audio_ssrc,
                                        streams = ready_streams.as_array().map(|items| items.len()).unwrap_or(0),
                                        "announced video capability"
                                    );
                                }
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
                                tracing::info!(
                                    audio_codec = data.get("audio_codec").and_then(|value| value.as_str()).unwrap_or(""),
                                    video_codec = data.get("video_codec").and_then(|value| value.as_str()).unwrap_or(""),
                                    "voice session description"
                                );
                                if let Some(sdp) = data.get("sdp").and_then(|v| v.as_str()) {
                                    server_sdp = sdp.to_owned();
                                    answer_deadline = Some(Instant::now() + Duration::from_millis(700));
                                    refresh_remote(
                                        &peer,
                                        &mut local_sdp,
                                        &server_sdp,
                                        &audio_streams,
                                        &video_streams,
                                        &mut applied_audio,
                                        &mut applied_video,
                                        &mut answered,
                                        answer_deadline,
                                        false,
                                    )
                                    .await?;
                                }
                                let wants = sink_wants(params.prefer_quality, &video_streams);
                                socket.send(Message::Text(wants.to_string().into())).await?;
                                if let Some(announce) = &video_announce {
                                    socket.send(Message::Text(announce.clone().into())).await?;
                                }
                            }
                            5 => {
                                let user_id = parse_id(payload.pointer("/d/user_id")).unwrap_or(0);
                                let ssrc = payload.pointer("/d/ssrc").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                if user_id != 0 && user_id != params.user_id && ssrc != 0 {
                                    book.audio.insert(ssrc, user_id);
                                    remember_audio(&mut audio_streams, user_id, ssrc);
                                    refresh_remote(
                                        &peer,
                                        &mut local_sdp,
                                        &server_sdp,
                                        &audio_streams,
                                        &video_streams,
                                        &mut applied_audio,
                                        &mut applied_video,
                                        &mut answered,
                                        answer_deadline,
                                        true,
                                    )
                                    .await?;
                                }
                            }
                            12 => {
                                handle_video_opcode(
                                    &payload["d"],
                                    &mut book,
                                    &mut audio_streams,
                                    &mut video_streams,
                                    params.user_id,
                                    &events,
                                    params.prefer_quality,
                                )
                                .await;
                                refresh_remote(
                                    &peer,
                                    &mut local_sdp,
                                    &server_sdp,
                                    &audio_streams,
                                    &video_streams,
                                    &mut applied_audio,
                                    &mut applied_video,
                                    &mut answered,
                                    answer_deadline,
                                    true,
                                )
                                .await?;
                                let wants = sink_wants(params.prefer_quality, &video_streams);
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
                            handle_dave_binary(opcode, payload, &mut dave, &mut socket).await?;
                        }
                    }
                    Message::Close(frame) => {
                        return Err(MediaError::Voice(format!("voice closed: {frame:?}")));
                    }
                    _ => {}
                }
            }
            _ = failed_rx.recv() => {
                return Err(MediaError::WebRtc("ICE failed before media could flow".into()));
            }
            packet = rtp_rx.recv() => {
                let Some((kind, packet)) = packet else { break };
                let header = &packet.header;
                let item = RtpPacket {
                    payload: packet.payload.to_vec(),
                    marker: header.marker,
                    payload_type: header.payload_type,
                };
                if kind == RTPCodecType::Video {
                    video_rtp = video_rtp.saturating_add(1);
                    video_wait = None;
                }
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
                        &mut av1_frames,
                        h264_payload,
                        &events,
                        &video_jobs,
                        &mut wait_for_keyframe,
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
    av1_frames: &mut HashMap<u32, Av1Assembler>,
    _h264_payload: u8,
    events: &mpsc::Sender<MediaEvent>,
    video_jobs: &mpsc::Sender<VideoJob>,
    wait_for_keyframe: &mut bool,
) {
    let Some(user_id) = (if kind == RTPCodecType::Audio {
        book.audio.get(&ssrc).copied()
    } else {
        book.video.get(&ssrc).copied().or_else(|| book.audio.get(&ssrc).copied())
    }) else {
        note_unknown(kind, ssrc);
        return;
    };
    if kind == RTPCodecType::Audio {
        let plain = match dave.decrypt(user_id, false, &packet.payload) {
            Ok(plain) => plain,
            Err(err) => {
                note_reject("audio decrypt", err);
                return;
            }
        };
        note_packet("audio", packet.payload_type, plain.len());
        match opus.decode(&plain) {
            Ok(pcm) if !pcm.is_empty() => {
                note_decoded("audio");
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
            Err(err) => note_reject("opus decode", err),
        }
        return;
    }
    note_packet("video", packet.payload_type, packet.payload.len());
    if rtx_like(packet.payload_type) || packet.payload.is_empty() {
        return;
    }
    if packet.payload_type == 101 {
        let frame = av1_frames.entry(ssrc).or_default().push(&packet.payload, packet.marker);
        let Some(access_unit) = frame else {
            return;
        };
        note_assembled(&access_unit);
        let plain = match dave.decrypt(user_id, true, &access_unit) {
            Ok(plain) => plain,
            Err(err) => {
                note_reject("video decrypt", format!("{err} bytes={}", access_unit.len()));
                *wait_for_keyframe = true;
                return;
            }
        };
        queue_video(video_jobs, wait_for_keyframe, user_id, plain);
        return;
    }
    let piece = if matches!(packet.payload_type, 103 | 105 | 125) {
        let depacketizer = assemblers.entry(ssrc).or_default();
        match depacketizer.depacketize(&Bytes::from(packet.payload.clone())) {
            Ok(piece) => piece.to_vec(),
            Err(err) => {
                note_reject("h264 depacketize", err);
                access_units.remove(&ssrc);
                return;
            }
        }
    } else {
        packet.payload.to_vec()
    };
    let unit = access_units.entry(ssrc).or_default();
    unit.extend_from_slice(&piece);
    if !packet.marker && unit.len() < 512 * 1024 {
        return;
    }
    let access_unit = std::mem::take(unit);
    note_assembled(&access_unit);
    let plain = match dave.decrypt(user_id, true, &access_unit) {
        Ok(plain) => plain,
        Err(err) => {
            note_reject("video decrypt", format!("{err} bytes={}", access_unit.len()));
            return;
        }
    };
    let decoded = h264.decode(&plain);
    match decoded {
        Ok(Some((bgra, width, height))) => {
            if frame_has_picture(&bgra) {
                note_decoded("video picture");
            } else {
                note_decoded("video black");
            }
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
        Err(err) => note_reject(if packet.payload_type == 101 { "av1 decode" } else { "h264 decode" }, err),
    }
}

fn note_unknown(kind: RTPCodecType, ssrc: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < 6 {
        tracing::info!(?kind, ssrc, "rtp arrived before its speaker was announced");
    }
}

fn note_reject(kind: &str, err: impl std::fmt::Display) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(kind, error = %err, "media frame rejected");
    }
}

struct VideoJob {
    user_id: u64,
    plain: Vec<u8>,
    reset: bool,
}

fn av1_sequence_header(frame: &[u8]) -> bool {
    frame.first().is_some_and(|byte| (byte >> 3) & 0x0f == 1)
}

fn queue_video(jobs: &mpsc::Sender<VideoJob>, wait_for_keyframe: &mut bool, user_id: u64, plain: Vec<u8>) {
    let key = av1_sequence_header(&plain);
    if *wait_for_keyframe && !key {
        return;
    }
    let job = VideoJob {
        user_id,
        plain,
        reset: *wait_for_keyframe,
    };
    match jobs.try_send(job) {
        Ok(()) => *wait_for_keyframe = false,
        Err(_) => *wait_for_keyframe = true,
    }
}

fn spawn_video_decoder(events: mpsc::Sender<MediaEvent>) -> mpsc::Sender<VideoJob> {
    let (tx, mut rx) = mpsc::channel::<VideoJob>(8);
    tokio::spawn(async move {
        let Ok(mut decoder) = Av1Decoder::new() else {
            tracing::warn!("av1 decoder unavailable");
            return;
        };
        while let Some(job) = rx.recv().await {
            if job.reset {
                if let Ok(fresh) = Av1Decoder::new() {
                    decoder = fresh;
                }
            }
            let head = job.plain.get(..4).map(|bytes| format!("{bytes:02x?}")).unwrap_or_default();
            match decoder.decode(&job.plain) {
                Ok(Some((bgra, width, height))) => {
                    if frame_has_picture(&bgra) {
                        note_decoded("video picture");
                    } else {
                        note_decoded("video black");
                    }
                    let _ = events
                        .send(MediaEvent::Video {
                            user_id: job.user_id,
                            bgra,
                            width,
                            height,
                        })
                        .await;
                }
                Ok(None) => {}
                Err(err) => {
                    note_reject("av1 decode", format!("{err} head={head}"));
                    if let Ok(fresh) = Av1Decoder::new() {
                        decoder = fresh;
                    }
                }
            }
        }
    });
    tx
}

fn note_packet(kind: &str, payload_type: u8, bytes: usize) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::info!(kind, payload_type, bytes, "rtp payload");
    }
}

fn note_assembled(frame: &[u8]) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < 4 {
        let tail = frame.len().saturating_sub(2);
        let marker = frame.get(tail..).map(|bytes| format!("{bytes:02x?}")).unwrap_or_default();
        let head = frame.get(..4).map(|bytes| format!("{bytes:02x?}")).unwrap_or_default();
        tracing::info!(bytes = frame.len(), head, marker, "assembled video frame");
    }
}

fn rtx_like(payload_type: u8) -> bool {
    matches!(payload_type, 97 | 99 | 102 | 104 | 106)
}

fn frame_has_picture(bgra: &[u8]) -> bool {
    bgra.chunks(4).step_by(48).any(|pixel| pixel.first().is_some_and(|value| *value > 12) || pixel.get(1).is_some_and(|value| *value > 12) || pixel.get(2).is_some_and(|value| *value > 12))
}

fn note_decoded(kind: &str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) < 3 {
        tracing::info!(kind, "decoded media frame");
    }
}

async fn handle_video_opcode(
    data: &Value,
    book: &mut SsrcBook,
    audio_streams: &mut Vec<AssignedSsrc>,
    video_streams: &mut Vec<AssignedSsrc>,
    self_id: u64,
    events: &mpsc::Sender<MediaEvent>,
    prefer_quality: u32,
) {
    let user_id = parse_id(data.get("user_id")).unwrap_or(0);
    if user_id == 0 || user_id == self_id {
        return;
    }
    if let Some(audio) = data.get("audio_ssrc").and_then(|value| value.as_u64()) {
        if audio > 0 {
            book.audio.insert(audio as u32, user_id);
            remember_audio(audio_streams, user_id, audio as u32);
        }
    }
    let video = data.get("video_ssrc").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
    let layers = data
        .get("streams")
        .and_then(|value| value.as_array())
        .map(|streams| {
            streams
                .iter()
                .filter_map(|stream| {
                    let ssrc = stream.get("ssrc").and_then(|value| value.as_u64())? as u32;
                    if ssrc == 0 {
                        return None;
                    }
                    let quality = stream.get("quality").and_then(|value| value.as_u64()).unwrap_or(100) as u32;
                    let rtx = stream.get("rtx_ssrc").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
                    let rid = stream.get("rid").and_then(|value| value.as_str()).unwrap_or("").to_owned();
                    let active = stream.get("active").and_then(|value| value.as_bool()).unwrap_or(false);
                    let width = stream.pointer("/max_resolution/width").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
                    let height = stream.pointer("/max_resolution/height").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
                    Some((ssrc, quality, rtx, rid, active, width, height))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (_, _, rtx, _, _, _, _) in &layers {
        if *rtx != 0 {
            book.rtx.insert(*rtx);
        }
    }
    let chosen = layers
        .iter()
        .filter(|(_, quality, _, _, _, _, _)| *quality <= prefer_quality.max(1))
        .max_by_key(|(_, quality, _, _, active, width, height)| (*quality, width.saturating_mul(*height), u8::from(*active)))
        .map(|(ssrc, _, _, _, _, _, _)| *ssrc)
        .or_else(|| layers.first().map(|(ssrc, _, _, _, _, _, _)| *ssrc))
        .unwrap_or(video);
    let camera_on = chosen != 0 && (video != 0 || !layers.is_empty());
    if video != 0 {
        book.video.insert(video, user_id);
    }
    for (ssrc, _, _, _, _, _, _) in &layers {
        book.video.insert(*ssrc, user_id);
    }
    let rtx = layers
        .iter()
        .find(|(ssrc, _, _, _, _, _, _)| *ssrc == chosen)
        .map(|(_, _, rtx, _, _, _, _)| *rtx)
        .filter(|rtx| *rtx != 0)
        .unwrap_or(0);
    if chosen != 0 {
        remember_video(video_streams, user_id, chosen, rtx);
    }
    let described = layers
        .iter()
        .map(|(ssrc, quality, _, rid, active, width, height)| format!("ssrc={ssrc} rid={rid} q={quality} active={active} {width}x{height}"))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(video, chosen, layers = layers.len(), described, "video ssrcs");
    if chosen == 0 && video == 0 {
        book.video.retain(|_, owner| *owner != user_id);
        video_streams.retain(|stream| stream.user_id != user_id);
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
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<(), MediaError> {
    match opcode {
        25 => dave.external_sender(payload)?,
        27 => {
            if payload.is_empty() {
                return Ok(());
            }
            let operation = payload[0];
            match dave.proposals(operation, &payload[1..]) {
                Ok(Some(commit)) => {
                    let mut body = commit.commit;
                    if let Some(welcome) = commit.welcome {
                        body.extend_from_slice(&welcome);
                    }
                    socket
                        .send(Message::Binary(client_binary(28, &body).into()))
                        .await?;
                    tracing::info!("MLS proposals accepted");
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(%err, "MLS proposals rejected"),
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

fn rtp_payload_offset(raw: &[u8]) -> Option<usize> {
    if raw.len() < 12 {
        return None;
    }
    let cc = (raw[0] & 0x0f) as usize;
    let extension = raw[0] & 0x10 != 0;
    let mut offset = 12 + cc * 4;
    if extension {
        if raw.len() < offset + 4 {
            return None;
        }
        let words = u16::from_be_bytes([raw[offset + 2], raw[offset + 3]]) as usize;
        offset += 4 + words * 4;
    }
    (offset <= raw.len()).then_some(offset)
}

/// webrtc-rs drops a payload when the padding bit is set and the last byte
/// equals the payload length. Discord video packets set that bit on real media,
/// so restore the bytes when they are not zero padding.
fn restore_padded_payload(raw: &[u8]) -> Option<Bytes> {
    let start = rtp_payload_offset(raw)?;
    if start >= raw.len() {
        return None;
    }
    let payload = &raw[start..];
    if payload.is_empty() {
        return None;
    }
    let pad = *payload.last()? as usize;
    if pad == 0 || pad > payload.len() {
        return None;
    }
    let padding_body = if pad <= 1 {
        &[][..]
    } else {
        &payload[payload.len() - pad..payload.len() - 1]
    };
    if padding_body.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some(Bytes::copy_from_slice(payload))
}

async fn prepare_peer(
    video_slots: usize,
    rtp_tx: mpsc::Sender<(RTPCodecType, webrtc::rtp::packet::Packet)>,
) -> Result<Arc<webrtc::peer_connection::RTCPeerConnection>, MediaError> {
    let mut media = MediaEngine::default();
    register_discord_payloads(&mut media)?;
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
    for _ in 0..4 {
        peer.add_transceiver_from_kind(RTPCodecType::Audio, Some(recvonly()))
            .await
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    }
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
            tracing::info!(?kind, ssrc = track.ssrc(), "track started");
            let mut raw = vec![0u8; 1500];
            loop {
                raw.fill(0);
                match track.read(&mut raw).await {
                    Ok((mut packet, _)) => {
                        let end = raw.iter().rposition(|byte| *byte != 0).map(|index| index + 1).unwrap_or(0);
                        if packet.payload.is_empty() && packet.header.padding {
                            if let Some(restored) = restore_padded_payload(&raw[..end]) {
                                packet.header.padding = false;
                                packet.payload = restored;
                            }
                        }
                        {
                            use std::sync::atomic::{AtomicU32, Ordering};
                            static VIDEO_SEEN: AtomicU32 = AtomicU32::new(0);
                            static AUDIO_SEEN: AtomicU32 = AtomicU32::new(0);
                            let seen = if kind == RTPCodecType::Video { &VIDEO_SEEN } else { &AUDIO_SEEN };
                            if seen.fetch_add(1, Ordering::Relaxed) < 4 {
                                let head = raw.get(..end.min(24)).unwrap_or(&[]);
                                tracing::info!(
                                    ?kind,
                                    pt = packet.header.payload_type,
                                    payload = packet.payload.len(),
                                    raw = end,
                                    padding = packet.header.padding,
                                    marker = packet.header.marker,
                                    head = %format!("{head:02x?}"),
                                    "inbound rtp"
                                );
                            }
                        }
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
    tracing::info!(av1 = sdp.contains("AV1"), h264 = sdp.contains("H264"), "local offer");
    Ok((fragment, Value::Array(json_codecs), sdp, h264, rtx))
}

fn inactive_video(audio_ssrc: u32, ready_streams: &Value) -> Value {
    let streams = ready_streams
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|stream| {
                    let ssrc = stream.get("ssrc").and_then(|value| value.as_u64())?;
                    let rtx = stream.get("rtx_ssrc").and_then(|value| value.as_u64()).unwrap_or(0);
                    Some(json!({
                        "type": stream.get("type").and_then(|value| value.as_str()).unwrap_or("video"),
                        "rid": stream.get("rid").and_then(|value| value.as_str()).unwrap_or("100"),
                        "ssrc": ssrc,
                        "rtx_ssrc": rtx,
                        "active": false,
                        "quality": 100,
                        "max_bitrate": 0,
                        "max_framerate": 0,
                        "max_resolution": {"type": "fixed", "width": 0, "height": 0}
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let video_ssrc = streams.first().and_then(|stream| stream.get("ssrc")).and_then(|value| value.as_u64()).unwrap_or(0);
    let rtx_ssrc = streams.first().and_then(|stream| stream.get("rtx_ssrc")).and_then(|value| value.as_u64()).unwrap_or(0);
    json!({
        "op": 12,
        "d": {
            "audio_ssrc": audio_ssrc,
            "video_ssrc": video_ssrc,
            "rtx_ssrc": rtx_ssrc,
            "streams": streams
        }
    })
}

fn sink_wants(quality: u32, video: &[AssignedSsrc]) -> Value {
    let quality = quality.min(100);
    let mut body = serde_json::Map::new();
    body.insert("any".into(), json!(quality));
    let mut pixels = serde_json::Map::new();
    for stream in video {
        if stream.ssrc == 0 {
            continue;
        }
        let key = stream.ssrc.to_string();
        body.insert(key.clone(), json!(quality));
        pixels.insert(key, json!(8_294_400));
    }
    if !pixels.is_empty() {
        body.insert("pixelCounts".into(), Value::Object(pixels));
    }
    json!({"op": 15, "d": body})
}

fn remember_audio(streams: &mut Vec<AssignedSsrc>, user_id: u64, ssrc: u32) {
    if streams.iter().any(|stream| stream.ssrc == ssrc) {
        return;
    }
    streams.push(AssignedSsrc { user_id, ssrc, rtx: 0 });
}

fn remember_video(streams: &mut Vec<AssignedSsrc>, user_id: u64, ssrc: u32, rtx: u32) {
    if let Some(existing) = streams.iter_mut().find(|stream| stream.user_id == user_id) {
        existing.ssrc = ssrc;
        existing.rtx = rtx;
        return;
    }
    streams.push(AssignedSsrc { user_id, ssrc, rtx });
}

async fn refresh_remote(
    peer: &webrtc::peer_connection::RTCPeerConnection,
    local_sdp: &mut String,
    server_sdp: &str,
    audio: &[AssignedSsrc],
    video: &[AssignedSsrc],
    applied_audio: &mut Vec<u32>,
    applied_video: &mut Vec<u32>,
    answered: &mut bool,
    answer_deadline: Option<Instant>,
    allow_early: bool,
) -> Result<(), MediaError> {
    if server_sdp.is_empty() {
        return Ok(());
    }
    let current_audio: Vec<u32> = audio.iter().map(|stream| stream.ssrc).collect();
    let current_video: Vec<u32> = video.iter().map(|stream| stream.ssrc).collect();
    if *answered && current_audio == *applied_audio && current_video == *applied_video {
        return Ok(());
    }
    if !*answered {
        let due = answer_deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if current_audio.is_empty() && current_video.is_empty() && !due {
            return Ok(());
        }
        if !allow_early && !due && current_video.is_empty() {
            return Ok(());
        }
    }
    publish_description(peer, local_sdp, server_sdp, audio, video, *answered).await?;
    *answered = true;
    *applied_audio = current_audio;
    *applied_video = current_video;
    Ok(())
}

async fn publish_description(
    peer: &webrtc::peer_connection::RTCPeerConnection,
    local_sdp: &mut String,
    server_sdp: &str,
    audio: &[AssignedSsrc],
    video: &[AssignedSsrc],
    renegotiate: bool,
) -> Result<(), MediaError> {
    if renegotiate {
        tracing::info!(audio = audio.len(), video = video.len(), "renegotiating for remote SSRCs");
        let offer = peer
            .create_offer(None)
            .await
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
        peer.set_local_description(offer)
            .await
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
        let mut gathering = peer.gathering_complete_promise().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), gathering.recv()).await;
        if let Some(local) = peer.local_description().await {
            *local_sdp = local.sdp;
        }
    }
    apply_answer(peer, local_sdp, server_sdp, audio, video).await
}

fn register_discord_payloads(media: &mut MediaEngine) -> Result<(), MediaError> {
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: Vec::new(),
                },
                payload_type: 120,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: Vec::new(),
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/AV1".to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: Vec::new(),
                },
                payload_type: 101,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    for payload_type in [102u8, 103, 104, 105, 125] {
        media
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_H264.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
                        rtcp_feedback: Vec::new(),
                    },
                    payload_type,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    }
    Ok(())
}

async fn apply_answer(
    peer: &webrtc::peer_connection::RTCPeerConnection,
    local_sdp: &str,
    server_sdp: &str,
    audio: &[AssignedSsrc],
    video: &[AssignedSsrc],
) -> Result<(), MediaError> {
    let answer = synthesize_answer(local_sdp, server_sdp, audio, video)
        .ok_or_else(|| MediaError::WebRtc("server SDP is missing ICE or DTLS".into()))?;
    let candidates = answer.lines().filter(|line| line.starts_with("a=candidate:")).count();
    tracing::info!(
        candidates,
        audio = audio.len(),
        video = video.len(),
        first_video = video.first().map(|stream| stream.ssrc).unwrap_or(0),
        sendonly = answer.matches("a=sendonly").count(),
        "remote ICE candidates installed"
    );
    if candidates == 0 {
        return Err(MediaError::WebRtc("server SDP contained no usable ICE candidate".into()));
    }
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
