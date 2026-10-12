use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
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

use crate::av1_ipc::Av1Supervisor;
use crate::dave::{client_binary, split_server_binary, DaveState, DAVE_VERSION};
use crate::decode::{strip_dave_supplemental, Av1Assembler, H264Decoder, OpusDecoder};
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

/// Newest decoded picture. Older frames are replaced, so a slow output cannot
/// leave the preview stuck on an image from seconds ago.
#[derive(Clone)]
pub struct VideoPicture {
    pub user_id: u64,
    pub bgra: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

pub struct MediaHandle {
    pub events: mpsc::Receiver<MediaEvent>,
    pub pictures: watch::Receiver<Option<VideoPicture>>,
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
    let (pictures_tx, pictures) = watch::channel(None);
    let child_cancel = cancel.clone();
    tokio::spawn(async move {
        if let Err(err) = run_connection(params, tx.clone(), pictures_tx, child_cancel).await {
            tracing::warn!(%err, "media connection ended");
            let _ = tx
                .send(MediaEvent::Disconnected {
                    reason: err.to_string(),
                })
                .await;
        }
    });
    MediaHandle { events, pictures, cancel }
}

async fn run_connection(
    params: ConnectParams,
    events: mpsc::Sender<MediaEvent>,
    pictures: watch::Sender<Option<VideoPicture>>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let endpoint = params.endpoint.trim_end_matches('/').trim_start_matches("wss://");
    let url = format!("wss://{endpoint}/?v=8");
    tracing::info!(url, ?params.kind, "connecting voice gateway");
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|err| MediaError::Voice(err.to_string()))?;

    let (rtp_tx, mut rtp_rx) = mpsc::channel::<(RTPCodecType, webrtc::rtp::packet::Packet)>(2048);
    let (video_jobs, mut av1_faults) = spawn_video_decoder(pictures.clone());
    let mut av1_dead = false;
    let av1_pli = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pending_av1_faults = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let mut keyframe_wait: HashSet<u32> = HashSet::new();
    let mut request_pli = false;
    let mut video_rtp = 0u32;
    let peer = prepare_peer(4, params.max_video_slots, rtp_tx).await?;
    let (failed_tx, mut failed_rx) = mpsc::channel::<()>(1);
    let peer_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready_flag = peer_ready.clone();
    peer.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let failed_notify = failed_tx.clone();
        let ready_flag = ready_flag.clone();
        Box::pin(async move {
            tracing::info!(?state, "peer connection state");
            if state == RTCPeerConnectionState::Connected {
                ready_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
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
    let mut opus_decoders: HashMap<u32, OpusDecoder> = HashMap::new();
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
    let mut voice_seq: Option<i64> = None;
    let mut video_announce: Option<String> = None;
    let mut answer_tick = interval(Duration::from_millis(200));
    answer_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut video_since: Option<Instant> = None;
    let mut resubscribed = false;
    let mut logged_receivers = false;
    let mut seen_ops: HashMap<u64, u8> = HashMap::new();

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
                    if av1_pli.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        request_pli = true;
                    }
                    keyframe_requests = keyframe_requests.saturating_add(1);
                    let ready = peer_ready.load(std::sync::atomic::Ordering::Relaxed);
                    // A PLI sent before SRTP is up uses an invalid sender SSRC and
                    // Discord sometimes stops forwarding video while leaving audio up.
                    if ready && video_since.is_none() {
                        video_since = Some(Instant::now());
                        request_pli = true;
                    }
                    if ready && !logged_receivers {
                        logged_receivers = true;
                        log_armed_receivers(&peer).await;
                    }
                    let missing_video = params.kind == MediaKind::GoLive
                        && video_rtp == 0
                        && video_streams.iter().any(|stream| stream.ssrc != 0);
                    if missing_video {
                        if let Some(start) = video_since {
                            if !resubscribed && start.elapsed() >= Duration::from_secs(2) {
                                resubscribed = true;
                                request_pli = true;
                                tracing::info!("resubscribing Go Live video");
                            }
                        }
                    }
                    if ready && (request_pli || keyframe_requests == 1 || keyframe_requests % 5 == 0) {
                        let wants = sink_wants(params.prefer_quality, &video_streams);
                        socket.send(Message::Text(wants.to_string().into())).await?;
                    }
                    // After video is flowing, a PLI every second forces intra frames
                    // and the sender scales the picture down. A decoder reset is
                    // the exception: that SSRC stays blocked until a sequence
                    // header arrives, so keep asking only for the stalled stream.
                    let recovering = !keyframe_wait.is_empty();
                    if ready && recovering && keyframe_requests % 5 == 0 {
                        request_pli = true;
                    }
                    let ask_keyframe = ready
                        && (request_pli || keyframe_requests <= 3 || (video_rtp == 0 && keyframe_requests % 5 == 0));
                    if ask_keyframe {
                        request_pli = false;
                        let sender_ssrc = local_audio_ssrc.max(1);
                        let waiting = keyframe_wait.iter().copied().filter(|ssrc| *ssrc != 0).collect::<Vec<_>>();
                        let targets = if waiting.is_empty() {
                            video_streams.iter().map(|stream| stream.ssrc).filter(|ssrc| *ssrc != 0).collect::<Vec<_>>()
                        } else {
                            waiting
                        };
                        for media_ssrc in targets {
                            let pli = PictureLossIndication { sender_ssrc, media_ssrc };
                            if let Err(err) = peer.write_rtcp(&[Box::new(pli)]).await {
                                tracing::warn!(%err, ssrc = media_ssrc, "PLI was not sent");
                            }
                        }
                        if keyframe_requests <= 20 || (recovering && keyframe_requests % 25 == 0) || (resubscribed && video_rtp == 0) {
                            tracing::info!(streams = video_streams.len(), sender_ssrc, recovering, "requested video keyframe");
                        }
                    }
                }
            }
            _ = heartbeat.tick(), if heartbeats_started => {
                let nonce = json!({
                    "op": 3,
                    "d": { "t": chrono_millis(), "seq_ack": voice_seq.unwrap_or(-1) }
                });
                socket.send(Message::Text(nonce.to_string().into())).await?;
            }
            incoming = socket.next() => {
                let Some(message) = incoming else { break };
                let message = message?;
                match message {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text)?;
                        if let Some(seq) = payload.get("seq").and_then(|value| value.as_i64()) {
                            voice_seq = Some(seq);
                        }
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
                                    socket.send(Message::Text(register_speaking(local_audio_ssrc).to_string().into())).await?;
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
                                if local_audio_ssrc != 0 {
                                    socket.send(Message::Text(register_speaking(local_audio_ssrc).to_string().into())).await?;
                                }
                            }
                            5 => {
                                let user_id = parse_id(payload.pointer("/d/user_id")).unwrap_or(0);
                                let ssrc = payload.pointer("/d/ssrc").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let speaking = payload.pointer("/d/speaking").and_then(|v| v.as_u64()).unwrap_or(0);
                                tracing::info!(user_id, ssrc, speaking, "speaking");
                                if user_id != 0 && user_id != params.user_id && ssrc != 0 {
                                    let retired = remember_audio(&mut audio_streams, &mut book, user_id, ssrc);
                                    drop_audio_state(&retired, &mut audio_jitter, &mut opus_decoders);
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
                            }
                            13 => {
                                let user_id = parse_id(payload.pointer("/d/user_id")).unwrap_or(0);
                                if user_id != 0 {
                                    let retired = forget_audio_user(&mut audio_streams, &mut book, user_id);
                                    drop_audio_state(&retired, &mut audio_jitter, &mut opus_decoders);
                                    tracing::info!(user_id, removed = retired.len(), "speaker left");
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
                                let before = audio_streams.iter().map(|stream| stream.ssrc).collect::<HashSet<_>>();
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
                                let retired = before
                                    .into_iter()
                                    .filter(|ssrc| !audio_streams.iter().any(|stream| stream.ssrc == *ssrc))
                                    .collect::<Vec<_>>();
                                drop_audio_state(&retired, &mut audio_jitter, &mut opus_decoders);
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
                            6 => {}
                            other => {
                                let count = seen_ops.entry(other).or_insert(0);
                                if *count < 3 {
                                    *count += 1;
                                    tracing::info!(op = other, "voice opcode");
                                }
                            }
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
            fault = av1_faults.recv(), if !av1_dead => {
                match fault {
                    Some(ssrc) => {
                        pending_av1_faults.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(ssrc);
                        av1_pli.store(true, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(ssrc, "av1 worker failed; waiting for a keyframe");
                    }
                    None => av1_dead = true,
                }
            }
            packet = rtp_rx.recv() => {
                let Some((kind, packet)) = packet else { break };
                for ssrc in pending_av1_faults.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain(..) {
                    av1_frames.remove(&ssrc);
                    keyframe_wait.insert(ssrc);
                }
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
                        &mut opus_decoders,
                        &mut h264,
                        &mut assemblers,
                        &mut access_units,
                        &mut av1_frames,
                        h264_payload,
                        &events,
                        &pictures,
                        &video_jobs,
                        &mut keyframe_wait,
                        &mut request_pli,
                        &mut video_rtp,
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
    opus_decoders: &mut HashMap<u32, OpusDecoder>,
    h264: &mut H264Decoder,
    assemblers: &mut HashMap<u32, H264Packet>,
    access_units: &mut HashMap<u32, Vec<u8>>,
    av1_frames: &mut HashMap<u32, Av1Assembler>,
    _h264_payload: u8,
    events: &mpsc::Sender<MediaEvent>,
    pictures: &watch::Sender<Option<VideoPicture>>,
    video_jobs: &mpsc::Sender<VideoJob>,
    keyframe_wait: &mut HashSet<u32>,
    request_pli: &mut bool,
    video_rtp: &mut u32,
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
        let decoder = match opus_decoders.entry(ssrc) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => match OpusDecoder::new() {
                Ok(decoder) => entry.insert(decoder),
                Err(err) => {
                    note_reject("opus decoder", err);
                    return;
                }
            },
        };
        match decoder.decode(&plain) {
            Ok(pcm) if !pcm.is_empty() => {
                note_audio_level(ssrc, user_id, &pcm);
                note_decoded("audio");
                enqueue_audio(
                    events,
                    MediaEvent::Audio {
                        user_id,
                        pcm,
                        sample_rate: 48_000,
                        channels: 2,
                    },
                );
            }
            Ok(_) => {}
            Err(err) => {
                note_reject("opus decode", err);
                opus_decoders.remove(&ssrc);
            }
        }
        return;
    }
    *video_rtp = video_rtp.saturating_add(1);
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
                keyframe_wait.insert(ssrc);
                av1_frames.remove(&ssrc);
                *request_pli = true;
                return;
            }
        };
        let plain = strip_dave_supplemental(&plain).map(|packet| packet.to_vec()).unwrap_or(plain);
        queue_video(video_jobs, keyframe_wait, user_id, ssrc, plain).await;
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
            let _ = pictures.send(Some(VideoPicture {
                user_id,
                bgra: Arc::<[u8]>::from(bgra),
                width,
                height,
            }));
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
    ssrc: u32,
    plain: Vec<u8>,
    reset: bool,
}

fn av1_sequence_header(frame: &[u8]) -> bool {
    crate::decode::contains_av1_sequence_header(frame)
}

async fn queue_video(jobs: &mpsc::Sender<VideoJob>, keyframe_wait: &mut HashSet<u32>, user_id: u64, ssrc: u32, plain: Vec<u8>) {
    let key = av1_sequence_header(&plain);
    let waiting = keyframe_wait.contains(&ssrc);
    if waiting && !key {
        return;
    }
    let job = VideoJob {
        user_id,
        ssrc,
        plain,
        reset: waiting,
    };
    // A delta after a decoder reset cannot be shown. A later access unit that
    // contains a sequence header is the recovery point, even when that header
    // is not the first OBU.
    if jobs.send(job).await.is_ok() && key {
        keyframe_wait.remove(&ssrc);
    }
}

fn spawn_video_decoder(pictures: watch::Sender<Option<VideoPicture>>) -> (mpsc::Sender<VideoJob>, mpsc::UnboundedReceiver<u32>) {
    let (tx, mut rx) = mpsc::channel::<VideoJob>(4);
    let (fault_tx, fault_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let supervisor = match Av1Supervisor::spawn() {
            Ok(supervisor) => std::sync::Arc::new(std::sync::Mutex::new(supervisor)),
            Err(err) => {
                tracing::warn!(%err, "av1 decoder unavailable");
                return;
            }
        };
        while let Some(job) = rx.recv().await {
            let supervisor = supervisor.clone();
            let ssrc = job.ssrc;
            let user_id = job.user_id;
            let decoded = tokio::task::spawn_blocking(move || {
                let mut supervisor = supervisor.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if job.reset {
                    supervisor.reset_stream(job.ssrc)?;
                }
                supervisor.decode(job.ssrc, &job.plain)
            })
            .await;
            match decoded {
                Ok(Ok(Some((bgra, width, height)))) => {
                    if frame_has_picture(&bgra) {
                        note_decoded("video picture");
                    } else {
                        note_decoded("video black");
                    }
                    let _ = pictures.send(Some(VideoPicture {
                        user_id,
                        bgra: Arc::<[u8]>::from(bgra),
                        width,
                        height,
                    }));
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    note_reject("av1 decode", err);
                    let _ = fault_tx.send(ssrc);
                }
                Err(err) => {
                    note_reject("av1 decode", err);
                    let _ = fault_tx.send(ssrc);
                }
            }
        }
    });
    (tx, fault_rx)
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

fn note_audio_level(ssrc: u32, user_id: u64, pcm: &[i16]) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let n = SEEN.fetch_add(1, Ordering::Relaxed);
    if n > 4 && n % 250 != 0 {
        return;
    }
    let peak = pcm.iter().map(|sample| sample.saturating_abs()).max().unwrap_or(0);
    tracing::info!(ssrc, user_id, peak, samples = pcm.len(), "decoded audio level");
}

fn enqueue_audio(events: &mpsc::Sender<MediaEvent>, event: MediaEvent) {
    use tokio::sync::mpsc::error::TrySendError;
    if let Err(err) = events.try_send(event) {
        if matches!(err, TrySendError::Full(_)) {
            note_audio_drop();
        }
    }
}

fn note_audio_drop() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static DROPPED: AtomicU64 = AtomicU64::new(0);
    let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if dropped <= 3 || dropped % 50 == 0 {
        tracing::warn!(dropped, "audio frame dropped because the bridge queue is full");
    }
}

fn drop_audio_state(ssrcs: &[u32], jitter: &mut HashMap<u32, JitterBuffer<RtpPacket>>, decoders: &mut HashMap<u32, OpusDecoder>) {
    for ssrc in ssrcs {
        jitter.remove(ssrc);
        decoders.remove(ssrc);
    }
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
            let retired = remember_audio(audio_streams, book, user_id, audio as u32);
            for ssrc in retired {
                book.audio.remove(&ssrc);
            }
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
        .map(|(ssrc, quality, rtx, rid, active, width, height)| {
            format!("ssrc={ssrc} rtx={rtx} rid={rid} q={quality} active={active} {width}x{height}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(
        video,
        audio = data.get("audio_ssrc").and_then(|value| value.as_u64()).unwrap_or(0),
        chosen,
        layers = layers.len(),
        described,
        "video ssrcs"
    );
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
    audio_slots: usize,
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
    for _ in 0..audio_slots {
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

fn register_speaking(ssrc: u32) -> Value {
    json!({
        "op": 5,
        "d": {
            "speaking": 0,
            "delay": 0,
            "ssrc": ssrc
        }
    })
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

fn remember_audio(streams: &mut Vec<AssignedSsrc>, book: &mut SsrcBook, user_id: u64, ssrc: u32) -> Vec<u32> {
    let mut retired = Vec::new();
    streams.retain(|stream| {
        let replace = stream.ssrc == ssrc && stream.user_id != user_id;
        if replace {
            retired.push(stream.ssrc);
        }
        !replace
    });
    if let Some(existing) = streams.iter_mut().find(|stream| stream.user_id == user_id) {
        if existing.ssrc != ssrc {
            retired.push(existing.ssrc);
            book.audio.remove(&existing.ssrc);
            existing.ssrc = ssrc;
        }
        book.audio.insert(ssrc, user_id);
        return retired;
    }
    streams.push(AssignedSsrc { user_id, ssrc, rtx: 0 });
    book.audio.insert(ssrc, user_id);
    retired
}

fn forget_audio_user(streams: &mut Vec<AssignedSsrc>, book: &mut SsrcBook, user_id: u64) -> Vec<u32> {
    let retired = streams
        .iter()
        .filter(|stream| stream.user_id == user_id)
        .map(|stream| stream.ssrc)
        .collect::<Vec<_>>();
    streams.retain(|stream| stream.user_id != user_id);
    for ssrc in &retired {
        book.audio.remove(ssrc);
    }
    retired
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
    let added = ensure_audio_slots(peer, audio.len()).await?;
    if renegotiate || added {
        tracing::info!(audio = audio.len(), video = video.len(), added, "renegotiating for remote SSRCs");
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

async fn ensure_audio_slots(peer: &webrtc::peer_connection::RTCPeerConnection, speakers: usize) -> Result<bool, MediaError> {
    let needed = speakers.max(4).min(32);
    let mut audio = 0usize;
    for transceiver in peer.get_transceivers().await {
        if transceiver.kind() == RTPCodecType::Audio {
            audio += 1;
        }
    }
    if audio >= needed {
        return Ok(false);
    }
    while audio < needed {
        let recvonly = RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: Vec::new(),
        };
        peer.add_transceiver_from_kind(RTPCodecType::Audio, Some(recvonly))
            .await
            .map_err(|err| MediaError::WebRtc(err.to_string()))?;
        audio += 1;
    }
    tracing::info!(audio, "added audio receive slots");
    Ok(true)
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
    log_audio_slots(&answer);
    if candidates == 0 {
        return Err(MediaError::WebRtc("server SDP contained no usable ICE candidate".into()));
    }
    let description = RTCSessionDescription::answer(answer)
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    peer.set_remote_description(description)
        .await
        .map_err(|err| MediaError::WebRtc(err.to_string()))?;
    let mut video_recv = 0u32;
    let mut video_other = 0u32;
    for transceiver in peer.get_transceivers().await {
        if transceiver.kind() != RTPCodecType::Video {
            continue;
        }
        match transceiver.direction() {
            RTCRtpTransceiverDirection::Recvonly | RTCRtpTransceiverDirection::Sendrecv => video_recv += 1,
            _ => video_other += 1,
        }
    }
    tracing::info!(video_recv, video_other, "video transceiver directions");
    Ok(())
}

async fn log_armed_receivers(peer: &webrtc::peer_connection::RTCPeerConnection) {
    for transceiver in peer.get_transceivers().await {
        if transceiver.kind() != RTPCodecType::Video {
            continue;
        }
        let receiver = transceiver.receiver().await;
        for track in receiver.tracks().await {
            if track.ssrc() == 0 {
                continue;
            }
            tracing::info!(
                ssrc = track.ssrc(),
                mid = transceiver.mid().as_deref().unwrap_or(""),
                "video receiver armed"
            );
        }
    }
}

fn log_audio_slots(answer: &str) {
    let mut slot = 0u32;
    let mut mid = String::new();
    let mut direction = String::new();
    let mut ssrc = 0u32;
    let mut audio = false;
    let flush = |slot: &mut u32, mid: &str, direction: &str, ssrc: u32, audio: bool| {
        if ssrc == 0 {
            return;
        }
        if audio {
            tracing::info!(slot = *slot, mid, direction, ssrc, "audio receive slot");
        } else {
            tracing::info!(mid, direction, ssrc, "video receive slot");
        }
    };
    for line in answer.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            flush(&mut slot, &mid, &direction, ssrc, audio);
            audio = rest.starts_with("audio");
            if audio {
                slot += 1;
            }
            mid.clear();
            direction.clear();
            ssrc = 0;
        } else if let Some(value) = line.strip_prefix("a=mid:") {
            mid = value.to_owned();
        } else if matches!(line, "a=sendonly" | "a=recvonly" | "a=inactive" | "a=sendrecv") {
            direction = line.trim_start_matches("a=").to_owned();
        } else if let Some(value) = line.strip_prefix("a=ssrc:") {
            if ssrc == 0 {
                ssrc = value.split_whitespace().next().and_then(|text| text.parse().ok()).unwrap_or(0);
            }
        }
    }
    flush(&mut slot, &mid, &direction, ssrc, audio);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_an_audio_ssrc_retires_the_old_one() {
        let mut streams = Vec::new();
        let mut book = SsrcBook::new();
        assert!(remember_audio(&mut streams, &mut book, 1, 11).is_empty());
        let retired = remember_audio(&mut streams, &mut book, 1, 99);
        assert_eq!(retired, vec![11]);
        assert_eq!(streams[0].ssrc, 99);
        assert!(!book.audio.contains_key(&11));
        assert_eq!(book.audio.get(&99), Some(&1));
    }

    #[test]
    fn a_leaving_speaker_drops_their_assignment() {
        let mut streams = Vec::new();
        let mut book = SsrcBook::new();
        remember_audio(&mut streams, &mut book, 1, 11);
        remember_audio(&mut streams, &mut book, 2, 22);
        let retired = forget_audio_user(&mut streams, &mut book, 1);
        assert_eq!(retired, vec![11]);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].ssrc, 22);
    }

    #[test]
    fn a_full_audio_queue_drops_the_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let event = MediaEvent::Audio {
            user_id: 1,
            pcm: vec![1],
            sample_rate: 48_000,
            channels: 2,
        };
        enqueue_audio(&tx, event.clone());
        enqueue_audio(&tx, event);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_sequence_header_after_the_first_obu_is_a_keyframe() {
        let temporal = [0x12u8, 0x00];
        let sequence = [0x0au8, 0x01, 0xaa];
        assert!(!av1_sequence_header(&temporal));
        assert!(av1_sequence_header(&[temporal.as_slice(), sequence.as_slice()].concat()));
        assert!(!av1_sequence_header(&[0x30, 0xd1, 0x7b, 0x7e]));
    }

    #[test]
    fn opus_decoders_stay_independent() {
        let mut first = OpusDecoder::new().unwrap();
        let mut second = OpusDecoder::new().unwrap();
        let _ = first.decode(&[0, 1, 2, 3]);
        assert!(second.decode(&[]).unwrap().is_empty());
    }
}
