use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;

use crate::GatewayError;

#[derive(Debug, Clone)]
pub struct VoiceEndpoint {
    pub guild_id: Option<u64>,
    pub channel_id: Option<u64>,
    pub endpoint: String,
    pub token: String,
    pub session_id: String,
    pub server_id: u64,
}

#[derive(Debug, Clone)]
pub struct StreamEndpoint {
    pub stream_key: String,
    pub endpoint: String,
    pub token: String,
    pub rtc_server_id: u64,
    pub user_id: u64,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct VoiceMember {
    pub user_id: u64,
    pub guild_id: Option<u64>,
    pub display_name: String,
    pub channel_id: Option<u64>,
    pub avatar: Option<String>,
    pub self_video: bool,
    pub self_stream: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
}

#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Ready {
        user_id: u64,
        username: String,
        avatar: Option<String>,
    },
    GuildUpsert(crate::directory::GuildRecord),
    GuildRemoved {
        guild_id: u64,
    },
    ChannelUpsert {
        guild_id: u64,
        channel: crate::directory::ChannelRecord,
    },
    ChannelRemoved {
        guild_id: u64,
        channel_id: u64,
    },
    VoiceReady(VoiceEndpoint),
    VoiceMember(VoiceMember),
    StreamAnnounced {
        stream_key: String,
        user_id: u64,
        paused: bool,
    },
    StreamAvailable(StreamEndpoint),
    StreamUpdated { stream_key: String, paused: bool },
    StreamEnded { stream_key: String },
    Disconnected { reason: String },
}

#[derive(Debug, Clone)]
pub enum GatewayCommand {
    JoinVoice {
        guild_id: u64,
        channel_id: u64,
        self_mute: bool,
        self_deaf: bool,
    },
    WatchStream { stream_key: String },
    LeaveVoice { guild_id: u64 },
    Shutdown,
}

pub struct GatewayHandle {
    pub events: mpsc::Receiver<GatewayEvent>,
    pub commands: mpsc::Sender<GatewayCommand>,
}

pub fn spawn_gateway(token: String, client_build_number: u32) -> GatewayHandle {
    let (event_tx, events) = mpsc::channel(8192);
    let (commands, command_rx) = mpsc::channel(32);
    tokio::spawn(async move {
        if let Err(err) = run(token, client_build_number, event_tx.clone(), command_rx).await {
            tracing::error!(%err, "gateway stopped");
            let _ = event_tx
                .send(GatewayEvent::Disconnected {
                    reason: err.to_string(),
                })
                .await;
        }
    });
    GatewayHandle { events, commands }
}

struct Session {
    id: Option<String>,
    sequence: Option<u64>,
    resume_url: Option<String>,
    self_user_id: Option<u64>,
    voice_token: Option<(Option<u64>, String, String)>,
    voice_session_id: Option<(u64, String)>,
    voice_join: Option<VoiceJoin>,
    pending_streams: Vec<PendingStream>,
}

struct VoiceJoin {
    guild_id: u64,
    channel_id: u64,
    self_mute: bool,
    self_deaf: bool,
}

struct PendingStream {
    stream_key: String,
    rtc_server_id: Option<u64>,
    endpoint: Option<String>,
    token: Option<String>,
    paused: bool,
}

async fn run(
    token: String,
    client_build_number: u32,
    events: mpsc::Sender<GatewayEvent>,
    mut commands: mpsc::Receiver<GatewayCommand>,
) -> Result<(), GatewayError> {
    let build = if client_build_number == 0 {
        fetch_build_number().await.unwrap_or(366_000)
    } else {
        client_build_number
    };
    let mut attempt = 0u32;
    let mut session = Session {
        id: None,
        sequence: None,
        resume_url: None,
        self_user_id: None,
        voice_token: None,
        voice_session_id: None,
        voice_join: None,
        pending_streams: Vec::new(),
    };
    loop {
        let url = if let (Some(resume), Some(_)) = (&session.resume_url, &session.id) {
            resume.clone()
        } else {
            fetch_gateway_url(&token).await?
        };
        match connect_once(&token, build, &url, &events, &mut commands, &mut session).await {
            Ok(GatewayCommand::Shutdown) => return Ok(()),
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(%err, "gateway connection lost");
                let _ = events
                    .send(GatewayEvent::Disconnected {
                        reason: err.to_string(),
                    })
                    .await;
            }
        }
        attempt = attempt.saturating_add(1);
        let delay = Duration::from_millis(500 * 2u64.saturating_pow(attempt.min(5)));
        sleep(delay).await;
        if attempt > 8 && session.id.is_none() {
            return Err(GatewayError::Message("gave up reconnecting the gateway".into()));
        }
    }
}

async fn connect_once(
    token: &str,
    build: u32,
    url: &str,
    events: &mpsc::Sender<GatewayEvent>,
    commands: &mut mpsc::Receiver<GatewayCommand>,
    session: &mut Session,
) -> Result<GatewayCommand, GatewayError> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
    let mut heartbeat = interval(Duration::from_secs(40));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut identified = false;
    loop {
        tokio::select! {
            incoming = socket.next() => {
                let Some(message) = incoming else { break };
                let message = message?;
                match message {
                    Message::Text(text) => {
                        let payload: Value = serde_json::from_str(&text)?;
                        if let Some(interval_ms) = hello_interval(&payload) {
                            heartbeat = interval(Duration::from_millis(interval_ms));
                            heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                            if !identified {
                                let identify = if session.id.is_some() && session.sequence.is_some() {
                                    json!({
                                        "op": 6,
                                        "d": {
                                            "token": token,
                                            "session_id": session.id,
                                            "seq": session.sequence
                                        }
                                    })
                                } else {
                                    identify_payload(token, build)
                                };
                                socket.send(Message::Text(identify.to_string().into())).await?;
                                identified = true;
                                // Voice joins are owned by the bridge so several channels can be
                                // queued one VoiceReady at a time after READY.
                                replay_streams(events, session).await;
                            }
                        }
                        handle_payload(&payload, events, session).await?;
                    }
                    Message::Close(frame) => {
                        return Err(GatewayError::Message(format!("gateway closed: {frame:?}")));
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick(), if identified => {
                let beat = json!({"op": 1, "d": session.sequence});
                socket.send(Message::Text(beat.to_string().into())).await?;
            }
            command = commands.recv(), if identified => {
                let Some(command) = command else {
                    return Ok(GatewayCommand::Shutdown);
                };
                match &command {
                    GatewayCommand::Shutdown => return Ok(GatewayCommand::Shutdown),
                    GatewayCommand::JoinVoice { guild_id, channel_id, self_mute, self_deaf } => {
                        let join = VoiceJoin {
                            guild_id: *guild_id,
                            channel_id: *channel_id,
                            self_mute: *self_mute,
                            self_deaf: *self_deaf,
                        };
                        let payload = voice_state_payload(&join);
                        session.voice_join = Some(join);
                        socket.send(Message::Text(payload.to_string().into())).await?;
                    }
                    GatewayCommand::LeaveVoice { guild_id } => {
                        session.voice_join = None;
                        let payload = json!({
                            "op": 4,
                            "d": {
                                "guild_id": guild_id.to_string(),
                                "channel_id": Value::Null,
                                "self_mute": true,
                                "self_deaf": false
                            }
                        });
                        socket.send(Message::Text(payload.to_string().into())).await?;
                    }
                    GatewayCommand::WatchStream { stream_key } => {
                        let payload = json!({"op": 20, "d": {"stream_key": stream_key}});
                        socket.send(Message::Text(payload.to_string().into())).await?;
                    }
                }
            }
        }
    }
    Err(GatewayError::Message("gateway socket ended".into()))
}

async fn handle_payload(
    payload: &Value,
    events: &mpsc::Sender<GatewayEvent>,
    session: &mut Session,
) -> Result<(), GatewayError> {
    if let Some(seq) = payload.get("s").and_then(|value| value.as_u64()) {
        session.sequence = Some(seq);
    }
    let op = payload.get("op").and_then(|value| value.as_u64()).unwrap_or(0);
    if op == 7 || op == 9 {
        if op == 9 && !payload.get("d").and_then(|value| value.as_bool()).unwrap_or(false) {
            session.id = None;
        }
        return Err(GatewayError::Message("gateway requested reconnect".into()));
    }
    let Some(name) = payload.get("t").and_then(|value| value.as_str()) else {
        return Ok(());
    };
    let data = payload.get("d").cloned().unwrap_or(Value::Null);
    match name {
        "READY" => {
            session.id = data.get("session_id").and_then(|value| value.as_str()).map(str::to_owned);
            session.resume_url = data
                .get("resume_gateway_url")
                .and_then(|value| value.as_str())
                .map(|url| format!("{url}/?v=9&encoding=json"));
            let user = &data["user"];
            let user_id = parse_id(user.get("id")).unwrap_or(0);
            session.self_user_id = Some(user_id);
            let username = user
                .get("global_name")
                .and_then(|value| value.as_str())
                .or_else(|| user.get("username").and_then(|value| value.as_str()))
                .unwrap_or("self")
                .to_owned();
            let avatar = user
                .get("avatar")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            events
                .send(GatewayEvent::Ready {
                    user_id,
                    username,
                    avatar,
                })
                .await
                .ok();
        }
        "GUILD_CREATE" | "GUILD_UPDATE" => {
            if name == "GUILD_CREATE" && data.get("unavailable").and_then(|value| value.as_bool()) == Some(true) {
                return Ok(());
            }
            if let Some(guild) = crate::directory::read_guild(&data) {
                let guild_id = guild.id;
                events.send(GatewayEvent::GuildUpsert(guild)).await.ok();
                if name == "GUILD_CREATE" {
                    if let Some(states) = data.get("voice_states").and_then(|value| value.as_array()) {
                        for state in states {
                            let member = voice_member(state, Some(guild_id));
                            events.send(GatewayEvent::VoiceMember(member)).await.ok();
                        }
                    }
                }
            }
        }
        "GUILD_DELETE" => {
            if data.get("unavailable").and_then(|value| value.as_bool()) == Some(true) {
                return Ok(());
            }
            if let Some(guild_id) = parse_id(data.get("id")) {
                events
                    .send(GatewayEvent::GuildRemoved { guild_id })
                    .await
                    .ok();
            }
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
            if let (Some(guild_id), Some(channel)) = (parse_id(data.get("guild_id")), crate::directory::read_channel(&data))
            {
                events
                    .send(GatewayEvent::ChannelUpsert { guild_id, channel })
                    .await
                    .ok();
            }
        }
        "CHANNEL_DELETE" => {
            if let (Some(guild_id), Some(channel_id)) = (parse_id(data.get("guild_id")), parse_id(data.get("id"))) {
                events
                    .send(GatewayEvent::ChannelRemoved { guild_id, channel_id })
                    .await
                    .ok();
            }
        }
        "VOICE_SERVER_UPDATE" => {
            let guild_id = parse_id(data.get("guild_id"));
            let token = data.get("token").and_then(|value| value.as_str()).unwrap_or("").to_owned();
            let endpoint = data
                .get("endpoint")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim_start_matches("wss://")
                .to_owned();
            session.voice_token = Some((guild_id, token, endpoint));
            emit_voice_if_ready(events, session).await;
        }
        "VOICE_STATE_UPDATE" => {
            let member = voice_member(&data, None);
            if session.self_user_id == Some(member.user_id) {
                if let Some(session_id) = data.get("session_id").and_then(|value| value.as_str()) {
                    session.voice_session_id = Some((member.user_id, session_id.to_owned()));
                    emit_voice_if_ready(events, session).await;
                }
            }
            events.send(GatewayEvent::VoiceMember(member)).await.ok();
        }
        "STREAM_CREATE" => {
            let stream_key = data.get("stream_key").and_then(|value| value.as_str()).unwrap_or("").to_owned();
            let rtc_server_id = parse_id(data.get("rtc_server_id"));
            let paused = data.get("paused").and_then(|value| value.as_bool()).unwrap_or(false);
            note_stream(session, &stream_key, rtc_server_id, None, None, paused);
            let user_id = crate::parse_stream_key(&stream_key).map(|(_, _, user_id)| user_id).unwrap_or(0);
            events
                .send(GatewayEvent::StreamAnnounced {
                    stream_key: stream_key.clone(),
                    user_id,
                    paused,
                })
                .await
                .ok();
            emit_stream_if_ready(events, session, &stream_key).await;
        }
        "STREAM_SERVER_UPDATE" => {
            let stream_key = data.get("stream_key").and_then(|value| value.as_str()).unwrap_or("").to_owned();
            let token = data.get("token").and_then(|value| value.as_str()).map(str::to_owned);
            let endpoint = data.get("endpoint").and_then(|value| value.as_str()).map(|value| {
                value.trim_start_matches("wss://").to_owned()
            });
            note_stream(session, &stream_key, None, endpoint, token, false);
            emit_stream_if_ready(events, session, &stream_key).await;
        }
        "STREAM_UPDATE" => {
            let stream_key = data.get("stream_key").and_then(|value| value.as_str()).unwrap_or("").to_owned();
            let paused = data.get("paused").and_then(|value| value.as_bool()).unwrap_or(false);
            if let Some(stream) = session.pending_streams.iter_mut().find(|stream| stream.stream_key == stream_key) {
                stream.paused = paused;
            }
            events
                .send(GatewayEvent::StreamUpdated { stream_key, paused })
                .await
                .ok();
        }
        "STREAM_DELETE" => {
            let stream_key = data.get("stream_key").and_then(|value| value.as_str()).unwrap_or("").to_owned();
            session.pending_streams.retain(|stream| stream.stream_key != stream_key);
            events
                .send(GatewayEvent::StreamEnded { stream_key })
                .await
                .ok();
        }
        _ => {}
    }
    Ok(())
}

async fn emit_voice_if_ready(events: &mpsc::Sender<GatewayEvent>, session: &mut Session) {
    let (Some((guild_id, token, endpoint)), Some((user_id, session_id))) =
        (session.voice_token.clone(), session.voice_session_id.clone())
    else {
        return;
    };
    if endpoint.is_empty() || token.is_empty() {
        return;
    }
    let server_id = guild_id.unwrap_or(user_id);
    let channel_id = session.voice_join.as_ref().map(|join| join.channel_id);
    let guild_id = guild_id.or_else(|| session.voice_join.as_ref().map(|join| join.guild_id));
    events
        .send(GatewayEvent::VoiceReady(VoiceEndpoint {
            guild_id,
            channel_id,
            endpoint,
            token,
            session_id,
            server_id,
        }))
        .await
        .ok();
    session.voice_token = None;
}

fn note_stream(
    session: &mut Session,
    stream_key: &str,
    rtc_server_id: Option<u64>,
    endpoint: Option<String>,
    token: Option<String>,
    paused: bool,
) {
    if stream_key.is_empty() {
        return;
    }
    if let Some(existing) = session
        .pending_streams
        .iter_mut()
        .find(|stream| stream.stream_key == stream_key)
    {
        if rtc_server_id.is_some() {
            existing.rtc_server_id = rtc_server_id;
        }
        if endpoint.is_some() {
            existing.endpoint = endpoint;
        }
        if token.is_some() {
            existing.token = token;
        }
        existing.paused = paused;
    } else {
        session.pending_streams.push(PendingStream {
            stream_key: stream_key.to_owned(),
            rtc_server_id,
            endpoint,
            token,
            paused,
        });
    }
}

async fn emit_stream_if_ready(events: &mpsc::Sender<GatewayEvent>, session: &mut Session, stream_key: &str) {
    let Some(stream) = session.pending_streams.iter().find(|stream| stream.stream_key == stream_key) else {
        return;
    };
    let (Some(rtc_server_id), Some(endpoint), Some(token)) =
        (stream.rtc_server_id, stream.endpoint.clone(), stream.token.clone())
    else {
        return;
    };
    let user_id = crate::parse_stream_key(stream_key).map(|(_, _, user_id)| user_id).unwrap_or(0);
    let paused = stream.paused;
    events
        .send(GatewayEvent::StreamAvailable(StreamEndpoint {
            stream_key: stream_key.to_owned(),
            endpoint,
            token,
            rtc_server_id,
            user_id,
            paused,
        }))
        .await
        .ok();
}

fn voice_state_payload(join: &VoiceJoin) -> Value {
    json!({
        "op": 4,
        "d": {
            "guild_id": join.guild_id.to_string(),
            "channel_id": join.channel_id.to_string(),
            "self_mute": join.self_mute,
            "self_deaf": join.self_deaf
        }
    })
}

async fn replay_streams(events: &mpsc::Sender<GatewayEvent>, session: &Session) {
    for stream in &session.pending_streams {
        if stream.paused {
            continue;
        }
        let user_id = crate::parse_stream_key(&stream.stream_key)
            .map(|(_, _, user_id)| user_id)
            .unwrap_or(0);
        events
            .send(GatewayEvent::StreamAnnounced {
                stream_key: stream.stream_key.clone(),
                user_id,
                paused: false,
            })
            .await
            .ok();
    }
}

fn voice_member(data: &Value, guild_hint: Option<u64>) -> VoiceMember {
    let user_id = parse_id(data.get("user_id")).unwrap_or(0);
    let user = data
        .get("member")
        .and_then(|member| member.get("user"))
        .or_else(|| data.get("user"));
    let nick = data
        .get("member")
        .and_then(|member| member.get("nick"))
        .and_then(|value| value.as_str());
    let global_name = user.and_then(|user| user.get("global_name")).and_then(|value| value.as_str());
    let username = user.and_then(|user| user.get("username")).and_then(|value| value.as_str());
    let display_name = nick.or(global_name).or(username).unwrap_or("").to_owned();
    VoiceMember {
        user_id,
        guild_id: parse_id(data.get("guild_id")).or(guild_hint),
        display_name,
        channel_id: parse_id(data.get("channel_id")),
        avatar: user
            .and_then(|user| user.get("avatar"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        self_video: data.get("self_video").and_then(|value| value.as_bool()).unwrap_or(false),
        self_stream: data.get("self_stream").and_then(|value| value.as_bool()).unwrap_or(false),
        self_mute: data.get("self_mute").and_then(|value| value.as_bool()).unwrap_or(false),
        self_deaf: data.get("self_deaf").and_then(|value| value.as_bool()).unwrap_or(false),
    }
}

pub(crate) fn parse_id(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse().ok())
            .or_else(|| value.as_u64())
    })
}

fn hello_interval(payload: &Value) -> Option<u64> {
    if payload.get("op").and_then(|value| value.as_u64()) != Some(10) {
        return None;
    }
    payload
        .pointer("/d/heartbeat_interval")
        .and_then(|value| value.as_u64())
}

fn identify_payload(token: &str, build: u32) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "capabilities": 30717,
            "properties": {
                "os": "Windows",
                "browser": "Discord Client",
                "device": "",
                "system_locale": "en-US",
                "browser_user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9163 Chrome/128.0.6613.186 Electron/32.2.2 Safari/537.36",
                "browser_version": "32.2.2",
                "os_version": "10.0.22631",
                "referrer": "",
                "referring_domain": "",
                "referrer_current": "",
                "referring_domain_current": "",
                "release_channel": "stable",
                "client_build_number": build,
                "client_event_source": Value::Null
            },
            "presence": {
                "status": "online",
                "since": 0,
                "activities": [],
                "afk": false
            },
            "compress": false,
            "client_state": { "guild_versions": {} }
        }
    })
}

async fn fetch_gateway_url(token: &str) -> Result<String, GatewayError> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) discord-omt-bridge")
        .build()?;
    let response = client
        .get("https://discord.com/api/v9/gateway")
        .header("Authorization", token)
        .send()
        .await?
        .error_for_status()?;
    let body: Value = response.json().await?;
    let url = body
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or("wss://gateway.discord.gg");
    Ok(format!("{url}/?v=9&encoding=json"))
}

async fn fetch_build_number() -> Result<u32, GatewayError> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) discord-omt-bridge")
        .build()?;
    let response = client
        .get("https://discord.com/api/updates/distributions/app/manifests/latest?channel=stable&platform=win&arch=x64")
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(GatewayError::Message("build lookup failed".into()));
    }
    let body: Value = response.json().await?;
    body.get("build_number")
        .or_else(|| body.get("buildNumber"))
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .ok_or_else(|| GatewayError::Message("build number missing".into()))
}
