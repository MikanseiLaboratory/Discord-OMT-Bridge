use std::collections::{HashMap, HashSet, VecDeque};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use discord_media::{spawn_media, ConnectParams, MediaEvent, MediaHandle, MediaKind};
use discord_user_gateway::{
    spawn_gateway, ChannelRecord, GatewayCommand, GatewayEvent, GuildRecord, StreamEndpoint, VoiceEndpoint,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::interval;

use crate::config::{Config, RuntimeSettings};
use crate::router::{log_sources, SourceKind, SourceRouter, Transport};
use crate::snapshot::{capture, ChannelState, GuildState, MemberState, PreviewFrame, SeatInput, Snapshot, UserState};

enum Bus {
    Gateway(GatewayEvent),
    Voice { channel_id: u64, event: MediaEvent },
    Stream { key: String, event: MediaEvent },
}

pub enum ControlCommand {
    Join { guild_id: u64, channel_id: u64 },
    Leave { guild_id: u64, channel_id: u64 },
    Assign {
        slot_id: String,
        channel_id: u64,
        user_id: u64,
        kind: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Clear {
        slot_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    AddOutput {
        id: String,
        name: String,
        kind: String,
        transport: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetTransport {
        id: String,
        transport: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveOutput {
        id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RenameOutput {
        id: String,
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Apply(RuntimeSettings),
    Previews {
        reply: oneshot::Sender<Vec<PreviewFrame>>,
    },
    Shutdown,
}

pub struct BridgeHandle {
    control: mpsc::Sender<ControlCommand>,
    snapshot: watch::Receiver<Snapshot>,
    thread: Option<JoinHandle<()>>,
}

impl BridgeHandle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    pub fn join(&self, guild_id: u64, channel_id: u64) -> Result<(), String> {
        self.send(ControlCommand::Join { guild_id, channel_id })
    }

    pub fn leave_channel(&self, guild_id: u64, channel_id: u64) -> Result<(), String> {
        self.send(ControlCommand::Leave { guild_id, channel_id })
    }

    pub fn assign_slot(&self, slot_id: String, channel_id: u64, user_id: u64, kind: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::Assign {
            slot_id,
            channel_id,
            user_id,
            kind,
            reply,
        })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn clear_slot(&self, slot_id: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::Clear { slot_id, reply })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn add_output(&self, id: String, name: String, kind: String, transport: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::AddOutput {
            id,
            name,
            kind,
            transport,
            reply,
        })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn remove_output(&self, id: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::RemoveOutput { id, reply })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn set_transport(&self, id: String, transport: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::SetTransport { id, transport, reply })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn rename_output(&self, id: String, name: String) -> Result<(), String> {
        let (reply, waiter) = oneshot::channel();
        self.send(ControlCommand::RenameOutput { id, name, reply })?;
        waiter
            .blocking_recv()
            .unwrap_or_else(|_| Err("ブリッジは停止しています".into()))
    }

    pub fn previews(&self) -> Vec<PreviewFrame> {
        let (reply, waiter) = oneshot::channel();
        if self.send(ControlCommand::Previews { reply }).is_err() {
            return Vec::new();
        }
        waiter.blocking_recv().unwrap_or_default()
    }

    pub fn apply(&self, runtime: RuntimeSettings) -> Result<(), String> {
        self.send(ControlCommand::Apply(runtime))
    }

    pub fn shutdown_and_join(mut self) {
        let _ = self.control.blocking_send(ControlCommand::Shutdown);
        drop(self.control);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn send(&self, command: ControlCommand) -> Result<(), String> {
        self.control
            .blocking_send(command)
            .map_err(|_| "ブリッジは停止しています".to_owned())
    }
}

struct Session {
    config: Config,
    token: String,
    initial_joins: Vec<(u64, u64)>,
    control: mpsc::Receiver<ControlCommand>,
    publish: Option<watch::Sender<Snapshot>>,
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let token = std::env::var("DISCORD_TOKEN")
        .map_err(|_| anyhow::anyhow!("環境変数 DISCORD_TOKEN にユーザートークンを設定してください"))?;
    let initial_joins = config.channel_targets();
    if initial_joins.is_empty() {
        anyhow::bail!("guild_id と channel_id、または channels を設定してください");
    }
    let (tx, rx) = mpsc::channel(8);
    let result = run_session(Session {
        config,
        token,
        initial_joins,
        control: rx,
        publish: None,
    })
    .await;
    drop(tx);
    result
}

pub fn spawn_session(token: String, config: Config) -> BridgeHandle {
    let (control_tx, control_rx) = mpsc::channel(32);
    let (publish, snapshot) = watch::channel(Snapshot::disconnected());
    let thread = std::thread::Builder::new()
        .name("discord-bridge".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let sender = publish.clone();
            if let Err(err) = runtime.block_on(run_session(Session {
                config,
                token,
                initial_joins: Vec::new(),
                control: control_rx,
                publish: Some(publish),
            })) {
                tracing::error!(%err, "bridge stopped");
                let mut snap = Snapshot::disconnected();
                snap.last_error = Some(err.to_string());
                let _ = sender.send(snap);
            }
        })
        .expect("bridge thread");
    BridgeHandle {
        control: control_tx,
        snapshot,
        thread: Some(thread),
    }
}

struct World {
    ready: bool,
    reconnecting: bool,
    self_user: Option<UserState>,
    guilds: Vec<GuildState>,
    members: HashMap<u64, MemberState>,
    last_error: Option<String>,
}

struct Seat {
    guild_id: u64,
    channel_id: u64,
    voice: Option<MediaHandle>,
    session_id: String,
    connected: bool,
}

struct MediaState {
    seats: Vec<Seat>,
    streams: HashMap<String, MediaHandle>,
    queue: VecDeque<(u64, u64)>,
    awaiting: Option<(u64, u64)>,
    focused: Option<(u64, u64)>,
    /// Drop one VoiceReady that belongs to a join we cancelled.
    discard_until: Option<Instant>,
    mute_pending: bool,
    /// Last channel sent with the gateway voice state.
    announced: Option<(u64, u64)>,
    autojoined: bool,
    watched: HashSet<String>,
}

async fn run_session(mut session: Session) -> anyhow::Result<()> {
    let gateway = spawn_gateway(session.token.clone(), session.config.client_build_number);
    let commands = gateway.commands.clone();
    let (bus_tx, mut bus_rx) = mpsc::channel(2048);
    let mut gateway_events = gateway.events;
    let gateway_bus = bus_tx.clone();
    tokio::spawn(async move {
        while let Some(event) = gateway_events.recv().await {
            if gateway_bus.send(Bus::Gateway(event)).await.is_err() {
                break;
            }
        }
    });

    let mut world = World {
        ready: false,
        reconnecting: false,
        self_user: None,
        guilds: Vec::new(),
        members: HashMap::new(),
        last_error: None,
    };
    let mut media = MediaState {
        seats: Vec::new(),
        streams: HashMap::new(),
        queue: VecDeque::new(),
        awaiting: None,
        focused: None,
        discard_until: None,
        mute_pending: false,
        announced: None,
        autojoined: false,
        watched: HashSet::new(),
    };
    for (guild_id, channel_id) in session.initial_joins.drain(..) {
        ensure_seat(&mut media, guild_id, channel_id);
        enqueue(&mut media, guild_id, channel_id);
    }

    let (dummy_tx, dummy_rx) = mpsc::channel(1);
    let mut control = std::mem::replace(&mut session.control, dummy_rx);
    drop(dummy_tx);

    let mut router = SourceRouter::open(&session.config)?;
    let mut self_id = 0u64;
    let mut monitor = interval(Duration::from_secs(5));
    let mut idle = interval(Duration::from_secs(1));

    loop {
        publish_mut(&session, &world, &media, &mut router);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                shutdown(&commands, &mut media).await;
                publish_mut(&session, &world, &media, &mut router);
                return Ok(());
            }
            _ = monitor.tick() => {
                let idle_after = Duration::from_secs(session.config.idle_timeout_secs);
                log_sources(&router.snapshots(idle_after));
            }
            _ = idle.tick() => {
                router.hold_open();
                if media.discard_until.is_some_and(|deadline| Instant::now() >= deadline) {
                    media.discard_until = None;
                    pump_joins(&world, &mut media, &commands, &session.config).await?;
                }
            }
            command = control.recv() => {
                match command {
                    None | Some(ControlCommand::Shutdown) => {
                        shutdown(&commands, &mut media).await;
                        if let Some(sender) = &session.publish {
                            let _ = sender.send(Snapshot::disconnected());
                        }
                        return Ok(());
                    }
                    Some(ControlCommand::Join { guild_id, channel_id }) => {
                        let connected = media
                            .seats
                            .iter()
                            .find(|seat| seat.guild_id == guild_id && seat.channel_id == channel_id)
                            .is_some_and(|seat| seat.connected);
                        if connected {
                            media.focused = Some((guild_id, channel_id));
                        } else {
                            ensure_seat(&mut media, guild_id, channel_id);
                            enqueue(&mut media, guild_id, channel_id);
                            world.last_error = None;
                            pump_joins(&world, &mut media, &commands, &session.config).await?;
                        }
                    }
                    Some(ControlCommand::Leave { guild_id, channel_id }) => {
                        let left_official = media.announced == Some((guild_id, channel_id));
                        leave_seat(&mut media, guild_id, channel_id);
                        if media.seats.is_empty() {
                            media.announced = None;
                            commands.send(GatewayCommand::LeaveVoice { guild_id }).await?;
                        } else if left_official {
                            if let Some((guild_id, channel_id)) = media.focused {
                                enqueue(&mut media, guild_id, channel_id);
                            }
                        }
                        pump_joins(&world, &mut media, &commands, &session.config).await?;
                    }
                    Some(ControlCommand::Assign { slot_id, channel_id, user_id, kind, reply }) => {
                        let parsed = if kind.trim().is_empty() {
                            Ok(None)
                        } else {
                            SourceKind::parse(&kind).map(Some)
                        };
                        let result = parsed.and_then(|kind| router.assign(&slot_id, channel_id, user_id, kind));
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::Clear { slot_id, reply }) => {
                        let result = router.clear(&slot_id);
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::AddOutput { id, name, kind, transport, reply }) => {
                        let result = SourceKind::parse(&kind).and_then(|parsed| {
                            Transport::parse(&transport)
                                .and_then(|transport| router.add_output(id, name, parsed, transport, 0, 0))
                        });
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::RemoveOutput { id, reply }) => {
                        let result = router.remove_output(&id);
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::SetTransport { id, transport, reply }) => {
                        let result = Transport::parse(&transport).and_then(|transport| router.set_transport(&id, transport));
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::RenameOutput { id, name, reply }) => {
                        let result = router.rename(&id, name);
                        if let Err(err) = &result {
                            world.last_error = Some(err.clone());
                        }
                        let _ = reply.send(result);
                    }
                    Some(ControlCommand::Previews { reply }) => {
                        let _ = reply.send(router.preview_frames());
                    }
                    Some(ControlCommand::Apply(runtime)) => {
                        let mute_changed = runtime.self_mute != session.config.self_mute
                            || runtime.self_deaf != session.config.self_deaf;
                        session.config.apply_runtime(&runtime);
                        if mute_changed {
                            media.mute_pending = true;
                            pump_joins(&world, &mut media, &commands, &session.config).await?;
                        }
                    }
                }
            }
            event = bus_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    Bus::Gateway(event) => {
                        handle_gateway(
                            event,
                            &session.config,
                            &commands,
                            &bus_tx,
                            &mut router,
                            &mut world,
                            &mut media,
                            &mut self_id,
                        ).await?;
                    }
                    Bus::Voice { channel_id, event } => {
                        let mut retry_guild = None;
                        if let MediaEvent::Disconnected { reason } = &event {
                            world.last_error = Some(reason.clone());
                            if let Some(seat) = media.seats.iter_mut().find(|seat| seat.channel_id == channel_id) {
                                seat.connected = false;
                                if let Some(handle) = seat.voice.take() {
                                    handle.cancel.cancel();
                                }
                                retry_guild = Some(seat.guild_id);
                            }
                        }
                        if let Some(guild_id) = retry_guild {
                            tracing::info!(channel_id, "voice media dropped; rejoining");
                            enqueue(&mut media, guild_id, channel_id);
                            media.discard_until = Some(Instant::now() + Duration::from_secs(2));
                        }
                        apply_media(&session.config, &mut router, self_id, channel_id, MediaKind::Voice, &event);
                    }
                    Bus::Stream { key, event } => {
                        let mut rewatch = false;
                        if let MediaEvent::Disconnected { reason } = &event {
                            world.last_error = Some(reason.clone());
                            rewatch = media.streams.remove(&key).is_some();
                        }
                        if rewatch {
                            tracing::info!(%key, "Go Live media dropped; watching again");
                            commands
                                .send(GatewayCommand::WatchStream { stream_key: key.clone() })
                                .await?;
                        }
                        let channel_id = discord_user_gateway::parse_stream_key(&key)
                            .map(|(_, channel_id, _)| channel_id)
                            .unwrap_or(0);
                        apply_media(&session.config, &mut router, self_id, channel_id, MediaKind::GoLive, &event);
                    }
                }
            }
        }
    }
    Ok(())
}

fn consider_autojoin(world: &World, media: &mut MediaState) {
    if media.autojoined || !world.ready {
        return;
    }
    let Ok(spec) = std::env::var("DISCORD_OMT_AUTOJOIN") else {
        return;
    };
    let Some((guild_part, channel_part)) = spec.split_once('|') else {
        return;
    };
    let guild_part = guild_part.trim();
    let channel_part = channel_part.trim();
    if guild_part.is_empty() || channel_part.is_empty() {
        return;
    }
    let Some(guild) = world.guilds.iter().find(|guild| guild.name.contains(guild_part)) else {
        return;
    };
    let Some(channel) = guild.channels.iter().find(|channel| {
        channel.kind == "voice" && channel.name.eq_ignore_ascii_case(channel_part)
    }) else {
        return;
    };
    media.autojoined = true;
    tracing::info!(guild = %guild.name, channel = %channel.name, "joining test channel");
    ensure_seat(media, guild.id, channel.id);
    enqueue(media, guild.id, channel.id);
}

fn ensure_seat(media: &mut MediaState, guild_id: u64, channel_id: u64) {
    if media
        .seats
        .iter()
        .any(|seat| seat.guild_id == guild_id && seat.channel_id == channel_id)
    {
        media.focused = Some((guild_id, channel_id));
        return;
    }
    media.seats.push(Seat {
        guild_id,
        channel_id,
        voice: None,
        session_id: String::new(),
        connected: false,
    });
    media.focused = Some((guild_id, channel_id));
}

fn enqueue(media: &mut MediaState, guild_id: u64, channel_id: u64) {
    let target = (guild_id, channel_id);
    if media.awaiting == Some(target) || media.queue.contains(&target) {
        return;
    }
    media.queue.push_back(target);
}

fn leave_seat(media: &mut MediaState, guild_id: u64, channel_id: u64) {
    let target = (guild_id, channel_id);
    media.queue.retain(|item| *item != target);
    if media.awaiting == Some(target) {
        media.awaiting = None;
        media.discard_until = Some(Instant::now() + Duration::from_secs(2));
    }
    if let Some(index) = media
        .seats
        .iter()
        .position(|seat| seat.guild_id == guild_id && seat.channel_id == channel_id)
    {
        if let Some(handle) = media.seats[index].voice.take() {
            handle.cancel.cancel();
        }
        media.seats.remove(index);
    }
    let keys: Vec<String> = media
        .streams
        .keys()
        .filter(|key| {
            discord_user_gateway::parse_stream_key(key)
                .is_some_and(|(stream_guild, stream_channel, _)| stream_guild == guild_id && stream_channel == channel_id)
        })
        .cloned()
        .collect();
    for key in keys {
        if let Some(handle) = media.streams.remove(&key) {
            handle.cancel.cancel();
        }
    }
    if media.focused == Some(target) {
        media.focused = media.seats.last().map(|seat| (seat.guild_id, seat.channel_id));
    }
}

async fn pump_joins(
    world: &World,
    media: &mut MediaState,
    commands: &mpsc::Sender<GatewayCommand>,
    config: &Config,
) -> anyhow::Result<()> {
    if !world.ready || media.awaiting.is_some() || media.discard_until.is_some() {
        return Ok(());
    }
    if let Some((guild_id, channel_id)) = media.queue.pop_front() {
        media.awaiting = Some((guild_id, channel_id));
        send_join(media, commands, config, guild_id, channel_id).await?;
        return Ok(());
    }
    if media.mute_pending {
        media.mute_pending = false;
        if let Some((guild_id, channel_id)) = media.focused {
            send_join(media, commands, config, guild_id, channel_id).await?;
        }
    }
    Ok(())
}

async fn send_join(
    media: &mut MediaState,
    commands: &mpsc::Sender<GatewayCommand>,
    config: &Config,
    guild_id: u64,
    channel_id: u64,
) -> anyhow::Result<()> {
    media.announced = Some((guild_id, channel_id));
    commands
        .send(GatewayCommand::JoinVoice {
            guild_id,
            channel_id,
            self_mute: config.self_mute,
            self_deaf: config.self_deaf,
        })
        .await?;
    Ok(())
}

fn publish_mut(session: &Session, world: &World, media: &MediaState, router: &mut SourceRouter) {
    let Some(sender) = &session.publish else {
        return;
    };
    let phase = if !world.ready {
        if world.reconnecting { "reconnecting" } else { "connecting" }
    } else if world.reconnecting {
        "reconnecting"
    } else if !media.seats.is_empty() {
        "voice"
    } else {
        "online"
    };
    let seats: Vec<SeatInput> = media
        .seats
        .iter()
        .map(|seat| SeatInput {
            guild_id: seat.guild_id,
            channel_id: seat.channel_id,
            connected: seat.connected,
            focused: media.focused == Some((seat.guild_id, seat.channel_id)),
        })
        .collect();
    let idle_after = Duration::from_secs(session.config.idle_timeout_secs);
    let slots = router.snapshots(idle_after);
    let snapshot = capture(
        phase,
        true,
        world.self_user.as_ref(),
        &world.guilds,
        &world.members,
        &seats,
        &slots,
        world.last_error.as_deref(),
        session.config.self_mute,
        session.config.self_deaf,
    );
    let _ = sender.send(snapshot);
}

async fn shutdown(commands: &mpsc::Sender<GatewayCommand>, media: &mut MediaState) {
    let mut guilds: Vec<u64> = media.seats.iter().map(|seat| seat.guild_id).collect();
    guilds.sort_unstable();
    guilds.dedup();
    for guild_id in guilds {
        let _ = commands.send(GatewayCommand::LeaveVoice { guild_id }).await;
    }
    let _ = commands.send(GatewayCommand::Shutdown).await;
    for seat in &mut media.seats {
        if let Some(handle) = seat.voice.take() {
            handle.cancel.cancel();
        }
    }
    for (_, handle) in media.streams.drain() {
        handle.cancel.cancel();
    }
}

async fn watch_live(
    commands: &mpsc::Sender<GatewayCommand>,
    world: &World,
    media: &mut MediaState,
    config: &Config,
    self_id: u64,
) -> anyhow::Result<()> {
    let connected: Vec<(u64, u64)> = media
        .seats
        .iter()
        .filter(|seat| seat.connected && !seat.session_id.is_empty())
        .map(|seat| (seat.guild_id, seat.channel_id))
        .collect();
    for member in world.members.values() {
        if !member.live || member.user_id == self_id || !config.allows(member.user_id) {
            continue;
        }
        let (Some(guild_id), Some(channel_id)) = (member.guild_id, member.channel_id) else {
            continue;
        };
        if !connected.contains(&(guild_id, channel_id)) {
            continue;
        }
        let stream_key = format!("guild:{guild_id}:{channel_id}:{}", member.user_id);
        if media.watched.insert(stream_key.clone()) {
            tracing::info!("watching Go Live");
            commands.send(GatewayCommand::WatchStream { stream_key }).await?;
        }
    }
    Ok(())
}

fn placeholder_name(name: &str) -> bool {
    let name = name.trim();
    name.is_empty() || name == "取得中" || name.starts_with("user-")
}

async fn handle_gateway(
    event: GatewayEvent,
    config: &Config,
    commands: &mpsc::Sender<GatewayCommand>,
    bus_tx: &mpsc::Sender<Bus>,
    router: &mut SourceRouter,
    world: &mut World,
    media: &mut MediaState,
    self_id: &mut u64,
) -> anyhow::Result<()> {
    match event {
        GatewayEvent::Ready { user_id, username, avatar } => {
            *self_id = user_id;
            world.ready = true;
            world.reconnecting = false;
            world.last_error = None;
            world.self_user = Some(UserState {
                id: user_id,
                name: username.clone(),
                avatar,
            });
            tracing::info!(user_id, %username, "gateway ready");
            consider_autojoin(world, media);
            let pending: Vec<(u64, u64)> = media
                .seats
                .iter()
                .filter(|seat| !seat.connected)
                .map(|seat| (seat.guild_id, seat.channel_id))
                .collect();
            for (guild_id, channel_id) in pending {
                enqueue(media, guild_id, channel_id);
            }
            pump_joins(world, media, commands, config).await?;
        }
        GatewayEvent::GuildUpsert(guild) => {
            upsert_guild(&mut world.guilds, guild);
            consider_autojoin(world, media);
            pump_joins(world, media, commands, config).await?;
        }
        GatewayEvent::GuildRemoved { guild_id } => {
            world.guilds.retain(|guild| guild.id != guild_id);
            world.members.retain(|_, member| member.guild_id != Some(guild_id));
        }
        GatewayEvent::ChannelUpsert { guild_id, channel } => {
            upsert_channel(&mut world.guilds, guild_id, channel);
            consider_autojoin(world, media);
            pump_joins(world, media, commands, config).await?;
        }
        GatewayEvent::ChannelRemoved { guild_id, channel_id } => {
            if let Some(guild) = world.guilds.iter_mut().find(|guild| guild.id == guild_id) {
                guild.channels.retain(|channel| channel.id != channel_id);
            }
        }
        GatewayEvent::Profile { user_id, name, avatar } => {
            if let Some(member) = world.members.get_mut(&user_id) {
                if placeholder_name(&member.name) {
                    member.name = name.clone();
                }
                if member.avatar.is_none() {
                    member.avatar = avatar;
                }
            }
            router.set_name(user_id, name);
        }
        GatewayEvent::VoiceMember(member) => {
            let name = if !member.display_name.is_empty() {
                member.display_name.clone()
            } else if let Some(existing) = world.members.get(&member.user_id).filter(|existing| !placeholder_name(&existing.name)) {
                existing.name.clone()
            } else if member.user_id == *self_id {
                world
                    .self_user
                    .as_ref()
                    .map(|user| user.name.clone())
                    .filter(|name| !placeholder_name(name))
                    .unwrap_or_else(|| "取得中".into())
            } else {
                "取得中".into()
            };
            if member.channel_id.is_none() {
                world.members.remove(&member.user_id);
            } else {
                world.members.insert(
                    member.user_id,
                    MemberState {
                        user_id: member.user_id,
                        guild_id: member.guild_id,
                        channel_id: member.channel_id,
                        name: name.clone(),
                        avatar: member.avatar,
                        camera: member.self_video,
                        live: member.self_stream,
                        mute: member.self_mute,
                        deaf: member.self_deaf,
                    },
                );
            }
            if !placeholder_name(&name) {
                router.set_name(member.user_id, name);
            }
            if let (Some(guild_id), Some(channel_id)) = (member.guild_id, member.channel_id) {
                if !member.self_stream {
                    media.watched.remove(&format!("guild:{guild_id}:{channel_id}:{}", member.user_id));
                }
            }
            watch_live(commands, world, media, config, *self_id).await?;
        }
        GatewayEvent::VoiceReady(endpoint) => {
            attach_voice(config, *self_id, endpoint, bus_tx, media)?;
            watch_live(commands, world, media, config, *self_id).await?;
            pump_joins(world, media, commands, config).await?;
        }
        GatewayEvent::StreamAnnounced { stream_key, user_id, paused } => {
            if paused || !config.allows(user_id) {
                return Ok(());
            }
            if media.streams.len() >= config.max_streams && !media.streams.contains_key(&stream_key) {
                tracing::warn!(%stream_key, "Go Live watch limit reached");
                return Ok(());
            }
            commands.send(GatewayCommand::WatchStream { stream_key }).await?;
        }
        GatewayEvent::StreamAvailable(endpoint) => {
            if endpoint.paused || !config.allows(endpoint.user_id) {
                return Ok(());
            }
            if media.streams.contains_key(&endpoint.stream_key) {
                return Ok(());
            }
            if media.streams.len() >= config.max_streams {
                tracing::warn!(stream_key = %endpoint.stream_key, "Go Live watch limit reached");
                return Ok(());
            }
            let Some((guild_id, channel_id, _)) = discord_user_gateway::parse_stream_key(&endpoint.stream_key) else {
                return Ok(());
            };
            let Some(session_id) = media
                .seats
                .iter()
                .find(|seat| seat.guild_id == guild_id && seat.channel_id == channel_id && !seat.session_id.is_empty())
                .map(|seat| seat.session_id.clone())
            else {
                tracing::warn!(stream_key = %endpoint.stream_key, "Go Live を見る親ボイスがまだありません");
                return Ok(());
            };
            let key = endpoint.stream_key.clone();
            let handle = start_stream(config, *self_id, channel_id, &session_id, endpoint, bus_tx.clone());
            media.streams.insert(key, handle);
        }
        GatewayEvent::StreamUpdated { stream_key, paused } => {
            if paused {
                if let Some(handle) = media.streams.remove(&stream_key) {
                    handle.cancel.cancel();
                }
            }
        }
        GatewayEvent::StreamEnded { stream_key } => {
            if let Some(handle) = media.streams.remove(&stream_key) {
                handle.cancel.cancel();
            }
        }
        GatewayEvent::Disconnected { reason } => {
            tracing::warn!(%reason, "gateway disconnected; it will retry");
            world.reconnecting = true;
            world.ready = false;
            world.last_error = Some(reason);
            media.awaiting = None;
            media.discard_until = None;
            for seat in &mut media.seats {
                seat.connected = false;
                if let Some(handle) = seat.voice.take() {
                    handle.cancel.cancel();
                }
            }
            for (_, handle) in media.streams.drain() {
                handle.cancel.cancel();
            }
        }
    }
    Ok(())
}

fn attach_voice(
    config: &Config,
    self_id: u64,
    endpoint: VoiceEndpoint,
    bus_tx: &mpsc::Sender<Bus>,
    media: &mut MediaState,
) -> anyhow::Result<()> {
    if media.discard_until.take().is_some() {
        tracing::info!("discarded a voice endpoint from a cancelled join");
        return Ok(());
    }
    let target = match (endpoint.guild_id, endpoint.channel_id) {
        (Some(guild_id), Some(channel_id)) => Some((guild_id, channel_id)),
        _ => media.awaiting,
    };
    let Some((guild_id, channel_id)) = target else {
        return Ok(());
    };
    let matches_awaiting = media.awaiting == Some((guild_id, channel_id));
    let Some(seat) = media
        .seats
        .iter_mut()
        .find(|seat| seat.guild_id == guild_id && seat.channel_id == channel_id)
    else {
        if matches_awaiting {
            media.awaiting = None;
        }
        return Ok(());
    };
    if let Some(handle) = seat.voice.take() {
        handle.cancel.cancel();
    }
    seat.session_id = endpoint.session_id.clone();
    seat.connected = true;
    seat.voice = Some(start_voice(config, self_id, channel_id, endpoint, bus_tx.clone()));
    if matches_awaiting {
        media.awaiting = None;
    }
    Ok(())
}

fn upsert_guild(guilds: &mut Vec<GuildState>, record: GuildRecord) {
    let channels = record.channels.map(|list| list.into_iter().map(channel_state).collect::<Vec<_>>());
    if let Some(existing) = guilds.iter_mut().find(|guild| guild.id == record.id) {
        existing.name = record.name;
        existing.icon = record.icon;
        if let Some(channels) = channels {
            existing.channels = channels;
        }
    } else {
        guilds.push(GuildState {
            id: record.id,
            name: record.name,
            icon: record.icon,
            channels: channels.unwrap_or_default(),
        });
    }
}

fn upsert_channel(guilds: &mut Vec<GuildState>, guild_id: u64, channel: ChannelRecord) {
    let Some(guild) = guilds.iter_mut().find(|guild| guild.id == guild_id) else {
        return;
    };
    let state = channel_state(channel);
    if let Some(existing) = guild.channels.iter_mut().find(|item| item.id == state.id) {
        *existing = state;
    } else {
        guild.channels.push(state);
    }
}

fn channel_state(channel: ChannelRecord) -> ChannelState {
    ChannelState {
        id: channel.id,
        name: channel.name,
        kind: channel.kind.as_str().to_owned(),
        position: channel.position,
        parent_id: channel.parent_id,
    }
}

fn start_voice(
    config: &Config,
    self_id: u64,
    channel_id: u64,
    endpoint: VoiceEndpoint,
    bus_tx: mpsc::Sender<Bus>,
) -> MediaHandle {
    let handle = spawn_media(ConnectParams {
        endpoint: endpoint.endpoint,
        token: endpoint.token,
        session_id: endpoint.session_id,
        server_id: endpoint.server_id,
        user_id: self_id,
        channel_id,
        kind: MediaKind::Voice,
        max_video_slots: config.max_video_slots,
        prefer_quality: config.prefer_quality,
    });
    forward_media(handle, bus_tx, VoiceRoute::Channel(channel_id))
}

fn start_stream(
    config: &Config,
    self_id: u64,
    channel_id: u64,
    voice_session_id: &str,
    endpoint: StreamEndpoint,
    bus_tx: mpsc::Sender<Bus>,
) -> MediaHandle {
    let key = endpoint.stream_key.clone();
    let handle = spawn_media(ConnectParams {
        endpoint: endpoint.endpoint,
        token: endpoint.token,
        session_id: voice_session_id.to_owned(),
        server_id: endpoint.rtc_server_id,
        user_id: self_id,
        channel_id,
        kind: MediaKind::GoLive,
        max_video_slots: config.max_video_slots,
        prefer_quality: config.prefer_quality,
    });
    tracing::info!(stream_key = %key, user_id = endpoint.user_id, "watching Go Live");
    forward_media(handle, bus_tx, VoiceRoute::Stream(key))
}

enum VoiceRoute {
    Channel(u64),
    Stream(String),
}

fn forward_media(handle: MediaHandle, bus_tx: mpsc::Sender<Bus>, route: VoiceRoute) -> MediaHandle {
    let cancel = handle.cancel.clone();
    let mut events = handle.events;
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let message = match &route {
                VoiceRoute::Channel(channel_id) => Bus::Voice {
                    channel_id: *channel_id,
                    event,
                },
                VoiceRoute::Stream(key) => Bus::Stream {
                    key: key.clone(),
                    event,
                },
            };
            if bus_tx.send(message).await.is_err() {
                break;
            }
        }
    });
    MediaHandle { events: idle_receiver(), cancel }
}

fn idle_receiver() -> mpsc::Receiver<MediaEvent> {
    mpsc::channel(1).1
}

fn apply_media(
    config: &Config,
    router: &mut SourceRouter,
    self_id: u64,
    channel_id: u64,
    kind: MediaKind,
    event: &MediaEvent,
) {
    match event {
        MediaEvent::Audio { user_id, pcm, sample_rate, channels } if *user_id != self_id => {
            let source = if kind == MediaKind::Voice {
                SourceKind::Audio
            } else {
                SourceKind::GoLive
            };
            router.publish_audio(config, channel_id, *user_id, source, pcm, *sample_rate, *channels);
        }
        MediaEvent::Video { user_id, bgra, width, height } if *user_id != self_id => {
            let source = if kind == MediaKind::Voice {
                SourceKind::Camera
            } else {
                SourceKind::GoLive
            };
            router.publish_video(config, channel_id, *user_id, source, bgra, *width, *height);
        }
        MediaEvent::Disconnected { reason } => {
            tracing::warn!(%reason, ?kind, channel_id, "media connection dropped");
        }
        _ => {}
    }
}
