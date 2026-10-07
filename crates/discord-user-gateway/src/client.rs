use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
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
    Profile {
        user_id: u64,
        name: String,
        avatar: Option<String>,
    },
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
                        handle_payload(&payload, events, session, token).await?;
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
    token: &str,
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
            let missing = publish_directory(events, data.get("guilds"), token).await;
            tracing::info!(guilds = missing.seen, without_channels = missing.ids.len(), "loaded guilds from ready");
            if !missing.ids.is_empty() {
                let events = events.clone();
                let token = token.to_owned();
                tokio::spawn(async move {
                    backfill_channels(token, missing.ids, events).await;
                });
            }
        }
        "READY_SUPPLEMENTAL" => {
            if let Some(guilds) = data.get("guilds").and_then(|value| value.as_array()) {
                for guild in guilds {
                    let guild_id = parse_id(guild.get("id"));
                    if let Some(states) = guild.get("voice_states").and_then(|value| value.as_array()) {
                        for state in states {
                            dispatch_voice(events, token, state, guild_id).await;
                        }
                    }
                }
            }
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
                            dispatch_voice(events, token, state, Some(guild_id)).await;
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
            dispatch_voice(events, token, &data, member.guild_id).await;
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

static PROFILE_CACHE: LazyLock<Mutex<HashMap<u64, (String, Option<String>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static PROFILE_PENDING: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

async fn dispatch_voice(events: &mpsc::Sender<GatewayEvent>, token: &str, data: &Value, guild_hint: Option<u64>) {
    let mut member = voice_member(data, guild_hint);
    if member.display_name.is_empty() {
        if let Some((name, avatar)) = cached_profile(member.user_id) {
            member.display_name = name;
            if member.avatar.is_none() {
                member.avatar = avatar;
            }
        } else {
            schedule_profile(token, &member, events.clone());
        }
    }
    events.send(GatewayEvent::VoiceMember(member)).await.ok();
}

fn cached_profile(user_id: u64) -> Option<(String, Option<String>)> {
    PROFILE_CACHE.lock().ok()?.get(&user_id).cloned()
}

fn schedule_profile(token: &str, member: &VoiceMember, events: mpsc::Sender<GatewayEvent>) {
    if member.user_id == 0 {
        return;
    }
    {
        let Ok(mut pending) = PROFILE_PENDING.lock() else {
            return;
        };
        if !pending.insert(member.user_id) {
            return;
        }
    }
    let token = token.to_owned();
    let user_id = member.user_id;
    let guild_id = member.guild_id;
    tokio::spawn(async move {
        let resolved = fetch_profile(&token, guild_id, user_id).await;
        if let Ok(mut pending) = PROFILE_PENDING.lock() {
            pending.remove(&user_id);
        }
        let Some((name, avatar)) = resolved else {
            return;
        };
        if let Ok(mut cache) = PROFILE_CACHE.lock() {
            cache.insert(user_id, (name.clone(), avatar.clone()));
        }
        events
            .send(GatewayEvent::Profile { user_id, name, avatar })
            .await
            .ok();
    });
}

async fn fetch_profile(token: &str, guild_id: Option<u64>, user_id: u64) -> Option<(String, Option<String>)> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) discord-omt-bridge")
        .build()
        .ok()?;
    if let Some(guild_id) = guild_id {
        let url = format!("https://discord.com/api/v9/guilds/{guild_id}/members/{user_id}");
        if let Some(profile) = request_profile(&client, token, &url, true).await {
            return Some(profile);
        }
    }
    let url = format!("https://discord.com/api/v9/users/{user_id}");
    request_profile(&client, token, &url, false).await
}

async fn request_profile(client: &reqwest::Client, token: &str, url: &str, member: bool) -> Option<(String, Option<String>)> {
    let response = client.get(url).header("Authorization", token).send().await.ok()?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "user profile lookup failed");
        return None;
    }
    let body: Value = response.json().await.ok()?;
    profile_from(&body, member)
}

fn profile_from(body: &Value, member: bool) -> Option<(String, Option<String>)> {
    let user = if member { body.get("user")? } else { body };
    let nick = member.then(|| body.get("nick")).flatten().and_then(|value| value.as_str());
    let global_name = user.get("global_name").and_then(|value| value.as_str());
    let username = user.get("username").and_then(|value| value.as_str());
    let name = [nick, global_name, username]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())?;
    let avatar = user.get("avatar").and_then(|value| value.as_str()).map(str::to_owned);
    Some((name.to_owned(), avatar))
}

pub(crate) fn parse_id(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse().ok())
            .or_else(|| value.as_u64())
    })
}

struct DirectoryLoad {
    seen: usize,
    ids: Vec<u64>,
}

async fn publish_directory(events: &mpsc::Sender<GatewayEvent>, guilds: Option<&Value>, token: &str) -> DirectoryLoad {
    let Some(guilds) = guilds.and_then(|value| value.as_array()) else {
        return DirectoryLoad { seen: 0, ids: Vec::new() };
    };
    let mut missing = Vec::new();
    let mut seen = 0usize;
    for guild in guilds {
        let Some(record) = crate::directory::read_guild(guild) else {
            continue;
        };
        seen += 1;
        let guild_id = record.id;
        let has_channels = record.channels.as_ref().is_some_and(|channels| !channels.is_empty());
        if let Some(states) = guild.get("voice_states").and_then(|value| value.as_array()) {
            for state in states {
                dispatch_voice(events, token, state, Some(guild_id)).await;
            }
        }
        events.send(GatewayEvent::GuildUpsert(record)).await.ok();
        if !has_channels {
            missing.push(guild_id);
        }
    }
    DirectoryLoad { seen, ids: missing }
}

async fn backfill_channels(token: String, guild_ids: Vec<u64>, events: mpsc::Sender<GatewayEvent>) {
    let Ok(client) = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9163 Chrome/128.0.6613.186 Electron/32.2.2 Safari/537.36")
        .build()
    else {
        return;
    };
    for guild_id in guild_ids {
        let response = match client
            .get(format!("https://discord.com/api/v9/guilds/{guild_id}/channels"))
            .header("Authorization", &token)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(guild_id, %err, "guild channel list failed");
                continue;
            }
        };
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(1.0);
            sleep(Duration::from_secs_f64(retry.min(5.0))).await;
            continue;
        }
        if !response.status().is_success() {
            tracing::warn!(guild_id, status = %response.status(), "guild channel list failed");
            continue;
        }
        let Ok(body) = response.json::<Value>().await else {
            continue;
        };
        let Some(channels) = body.as_array() else {
            continue;
        };
        let count = channels.len();
        for channel in channels {
            if let Some(record) = crate::directory::read_channel(channel) {
                events
                    .send(GatewayEvent::ChannelUpsert {
                        guild_id,
                        channel: record,
                    })
                    .await
                    .ok();
            }
        }
        tracing::info!(guild_id, channels = count, "loaded guild channels");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn voice_member_prefers_the_server_nickname() {
        let state = json!({
            "user_id": "42",
            "channel_id": "7",
            "member": {
                "nick": "司会",
                "user": { "id": "42", "username": "alice", "global_name": "Alice", "avatar": "abc" }
            }
        });
        let member = voice_member(&state, Some(9));
        assert_eq!(member.display_name, "司会");
        assert_eq!(member.user_id, 42);
        assert_eq!(member.avatar.as_deref(), Some("abc"));
    }

    #[test]
    fn profile_uses_global_name_when_there_is_no_nickname() {
        let body = json!({
            "user": { "username": "alice", "global_name": "Alice", "avatar": null },
            "nick": null
        });
        let (name, avatar) = profile_from(&body, true).unwrap();
        assert_eq!(name, "Alice");
        assert!(avatar.is_none());
    }
}
