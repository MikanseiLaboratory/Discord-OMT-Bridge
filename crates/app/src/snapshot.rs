use std::collections::HashMap;

use serde::Serialize;

use crate::router::SlotSnapshot;

#[derive(Debug, Clone, Serialize)]
pub struct PreviewFrame {
    pub user_id: String,
    pub name: String,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub rgba: String,
}

#[derive(Debug, Clone)]
pub struct UserState {
    pub id: u64,
    pub name: String,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelState {
    pub id: u64,
    pub name: String,
    pub kind: String,
    pub position: i64,
    pub parent_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct GuildState {
    pub id: u64,
    pub name: String,
    pub icon: Option<String>,
    pub channels: Vec<ChannelState>,
}

#[derive(Debug, Clone)]
pub struct MemberState {
    pub user_id: u64,
    pub guild_id: Option<u64>,
    pub channel_id: Option<u64>,
    pub name: String,
    pub avatar: Option<String>,
    pub camera: bool,
    pub live: bool,
    pub mute: bool,
    pub deaf: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub phase: String,
    pub connected: bool,
    pub self_user: Option<UserView>,
    pub guilds: Vec<GuildView>,
    pub seats: Vec<SeatView>,
    pub slots: Vec<SlotView>,
    pub last_error: Option<String>,
    pub ndi_available: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuildView {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub channels: Vec<ChannelView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub position: i64,
    pub parent_id: Option<String>,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberView {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
    pub camera: bool,
    pub live: bool,
    pub mute: bool,
    pub deaf: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeatView {
    pub guild_id: String,
    pub channel_id: String,
    pub connected: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub assigned_name: String,
    pub pinned: bool,
    pub live: bool,
    pub frames: u64,
    pub dropped: u64,
    pub fps: f64,
}

pub struct SeatInput {
    pub guild_id: u64,
    pub channel_id: u64,
    pub connected: bool,
    pub focused: bool,
}

impl Snapshot {
    pub fn disconnected() -> Self {
        Self {
            phase: "disconnected".into(),
            connected: false,
            self_user: None,
            guilds: Vec::new(),
            seats: Vec::new(),
            slots: Vec::new(),
            last_error: None,
            ndi_available: crate::ndi_compiled(),
            self_mute: true,
            self_deaf: false,
        }
    }
}

pub fn capture(
    phase: &str,
    connected: bool,
    self_user: Option<&UserState>,
    guilds: &[GuildState],
    members: &HashMap<u64, MemberState>,
    seats: &[SeatInput],
    slots: &[SlotSnapshot],
    last_error: Option<&str>,
    self_mute: bool,
    self_deaf: bool,
) -> Snapshot {
    let guilds = guilds
        .iter()
        .map(|guild| GuildView {
            id: guild.id.to_string(),
            name: guild.name.clone(),
            icon: guild.icon.clone(),
            channels: guild
                .channels
                .iter()
                .map(|channel| ChannelView {
                    id: channel.id.to_string(),
                    name: channel.name.clone(),
                    kind: channel.kind.clone(),
                    position: channel.position,
                    parent_id: channel.parent_id.map(|id| id.to_string()),
                    members: members
                        .values()
                        .filter(|member| member.channel_id == Some(channel.id))
                        .map(|member| MemberView {
                            id: member.user_id.to_string(),
                            name: if member.name.is_empty() {
                                "取得中".into()
                            } else {
                                member.name.clone()
                            },
                            avatar: member.avatar.clone(),
                            camera: member.camera,
                            live: member.live,
                            mute: member.mute,
                            deaf: member.deaf,
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    Snapshot {
        phase: phase.to_owned(),
        connected,
        self_user: self_user.map(|user| UserView {
            id: user.id.to_string(),
            name: user.name.clone(),
            avatar: user.avatar.clone(),
        }),
        guilds,
        seats: seats
            .iter()
            .map(|seat| SeatView {
                guild_id: seat.guild_id.to_string(),
                channel_id: seat.channel_id.to_string(),
                connected: seat.connected,
                focused: seat.focused,
            })
            .collect(),
        slots: slots
            .iter()
            .map(|slot| SlotView {
                id: slot.id.clone(),
                kind: slot.kind.clone(),
                name: slot.name.clone(),
                channel_id: slot.channel_id.map(|id| id.to_string()),
                user_id: slot.user_id.map(|id| id.to_string()),
                assigned_name: slot.assigned_name.clone(),
                pinned: slot.pinned,
                live: slot.live,
                frames: slot.frames,
                dropped: slot.dropped,
                fps: slot.fps,
            })
            .collect(),
        last_error: last_error.map(str::to_owned),
        ndi_available: crate::ndi_compiled(),
        self_mute,
        self_deaf,
    }
}
