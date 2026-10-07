use std::collections::HashMap;
use std::time::{Duration, Instant};

use egress::{AudioPublisher, FanoutPublisher, VideoPublisher};

use crate::config::Config;
use crate::snapshot::PreviewFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Audio,
    Camera,
    GoLive,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::Camera => "Camera",
            Self::GoLive => "GoLive",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Camera => "camera",
            Self::GoLive => "golive",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "audio" | "Audio" => Ok(Self::Audio),
            "camera" | "Camera" => Ok(Self::Camera),
            "golive" | "GoLive" => Ok(Self::GoLive),
            _ => Err("種類は音声、カメラ、Go Live のいずれかです".into()),
        }
    }

    fn mode(self) -> egress::SourceMode {
        match self {
            Self::Audio => egress::SourceMode::AudioOnly,
            Self::Camera => egress::SourceMode::VideoOnly,
            Self::GoLive => egress::SourceMode::AudioAndVideo,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Omt,
    Ndi,
}

impl Transport {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Omt => "omt",
            Self::Ndi => "ndi",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "omt" | "OMT" | "" => Ok(Self::Omt),
            "ndi" | "NDI" => Ok(Self::Ndi),
            _ => Err("送出先は OMT か NDI です".into()),
        }
    }

    fn flags(self) -> (bool, bool) {
        match self {
            Self::Omt => (true, false),
            Self::Ndi => (false, true),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedKey {
    pub channel_id: u64,
    pub user_id: u64,
    pub kind: SourceKind,
}

#[derive(Clone, Debug)]
pub struct SlotSnapshot {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub channel_id: Option<u64>,
    pub user_id: Option<u64>,
    pub assigned_name: String,
    pub pinned: bool,
    pub live: bool,
    pub frames: u64,
    pub dropped: u64,
    pub fps: f64,
}

#[derive(Clone, Copy)]
struct Binding {
    kind: SourceKind,
    assignment: Option<FeedKey>,
    pinned: bool,
}

struct OutputSlot {
    id: String,
    kind: SourceKind,
    transport: Transport,
    name: String,
    publisher: FanoutPublisher,
    binding: Binding,
    frames: u64,
    dropped: u64,
    last_media: Option<Instant>,
    last_output: Instant,
    window_start: Instant,
    window_frames: u64,
    fps: f64,
    width: u32,
    height: u32,
}

struct PreviewSlot {
    user_id: u64,
    kind: SourceKind,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    at: Instant,
}

pub struct SourceRouter {
    slots: Vec<OutputSlot>,
    names: HashMap<u64, String>,
    previews: Vec<PreviewSlot>,
    black: Vec<u8>,
    silence: Vec<i16>,
}

impl SourceRouter {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let mut router = Self {
            slots: Vec::new(),
            names: HashMap::new(),
            previews: Vec::new(),
            black: vec![0; 640 * 360 * 4],
            silence: vec![0; 960 * 2],
        };
        for route in &config.outputs {
            let kind = SourceKind::parse(&route.kind).map_err(anyhow::Error::msg)?;
            let transport = Transport::parse(&route.transport).map_err(anyhow::Error::msg)?;
            router
                .add_output(
                    route.id.clone(),
                    route.name.clone(),
                    kind,
                    transport,
                    route.channel_id,
                    route.user_id,
                )
                .map_err(anyhow::Error::msg)?;
        }
        tracing::info!(outputs = router.slots.len(), "opened matrix outputs");
        Ok(router)
    }

    pub fn set_name(&mut self, user_id: u64, name: String) {
        if !name.is_empty() {
            self.names.insert(user_id, name);
        }
    }

    pub fn publish_audio(
        &mut self,
        config: &Config,
        channel_id: u64,
        user_id: u64,
        kind: SourceKind,
        pcm: &[i16],
        sample_rate: u32,
        channels: u16,
    ) {
        if !config.allows(user_id) {
            return;
        }
        let feed = FeedKey { channel_id, user_id, kind };
        for index in self.indexes_for(feed) {
            let slot = &mut self.slots[index];
            if let Err(err) = slot.publisher.publish_audio(pcm, sample_rate, channels) {
                slot.dropped += 1;
                tracing::warn!(%err, slot = %slot.name, "audio publish failed");
            } else {
                slot.note_media();
            }
        }
    }

    pub fn publish_video(
        &mut self,
        config: &Config,
        channel_id: u64,
        user_id: u64,
        kind: SourceKind,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) {
        if !config.allows(user_id) {
            return;
        }
        self.remember_preview(user_id, kind, bgra, width, height);
        let feed = FeedKey { channel_id, user_id, kind };
        for index in self.indexes_for(feed) {
            let slot = &mut self.slots[index];
            slot.width = width;
            slot.height = height;
            if let Err(err) = slot.publisher.publish_video(bgra, width, height, (30, 1)) {
                slot.dropped += 1;
                tracing::warn!(%err, slot = %slot.name, "video publish failed");
            } else {
                slot.note_media();
            }
        }
    }

    pub fn add_output(
        &mut self,
        id: String,
        name: String,
        kind: SourceKind,
        transport: Transport,
        channel_id: u64,
        user_id: u64,
    ) -> Result<(), String> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err("名前を入力してください".into());
        }
        if self.slots.len() >= 64 {
            return Err("出力は64個までです".into());
        }
        if self.slots.iter().any(|slot| slot.id == id) {
            return Err("同じ出力が既にあります".into());
        }
        if self.slots.iter().any(|slot| slot.name == name) {
            return Err("同じ名前の出力があります".into());
        }
        let publisher = open_publisher(&name, kind, transport)?;
        let assignment = route_feed(kind, channel_id, user_id);
        tracing::info!(%name, kind = kind.slug(), transport = transport.slug(), "created output");
        self.slots.push(OutputSlot {
            id,
            kind,
            transport,
            name,
            publisher,
            binding: Binding {
                kind,
                assignment,
                pinned: assignment.is_some(),
            },
            frames: 0,
            dropped: 0,
            last_media: None,
            last_output: Instant::now(),
            window_start: Instant::now(),
            window_frames: 0,
            fps: 0.0,
            width: 640,
            height: 360,
        });
        Ok(())
    }

    pub fn remove_output(&mut self, id: &str) -> Result<(), String> {
        let Some(index) = self.slots.iter().position(|slot| slot.id == id) else {
            return Err("出力が見つかりません".into());
        };
        self.slots.remove(index);
        Ok(())
    }

    pub fn rename(&mut self, id: &str, name: String) -> Result<(), String> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err("名前を入力してください".into());
        }
        if self.slots.iter().any(|slot| slot.name == name && slot.id != id) {
            return Err("同じ名前の出力があります".into());
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == id) else {
            return Err("出力が見つかりません".into());
        };
        if slot.name == name {
            return Ok(());
        }
        let publisher = open_publisher(&name, slot.kind, slot.transport)?;
        slot.publisher = publisher;
        slot.name = name;
        slot.last_output = Instant::now().checked_sub(Duration::from_secs(2)).unwrap_or_else(Instant::now);
        Ok(())
    }

    pub fn assign(
        &mut self,
        slot_id: &str,
        channel_id: u64,
        user_id: u64,
        kind: Option<SourceKind>,
    ) -> Result<(), String> {
        let Some(index) = self.slots.iter().position(|slot| slot.id == slot_id) else {
            return Err("出力が見つかりません".into());
        };
        if let Some(kind) = kind {
            if self.slots[index].kind != kind {
                self.reopen(index, kind, self.slots[index].transport)?;
            }
        }
        let kind = self.slots[index].kind;
        let assignment = route_feed(kind, channel_id, user_id);
        self.slots[index].binding.assignment = assignment;
        self.slots[index].binding.pinned = assignment.is_some();
        Ok(())
    }

    pub fn set_transport(&mut self, slot_id: &str, transport: Transport) -> Result<(), String> {
        let Some(index) = self.slots.iter().position(|slot| slot.id == slot_id) else {
            return Err("出力が見つかりません".into());
        };
        if self.slots[index].transport == transport {
            return Ok(());
        }
        let kind = self.slots[index].kind;
        self.reopen(index, kind, transport)
    }

    pub fn clear(&mut self, slot_id: &str) -> Result<(), String> {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) else {
            return Err("出力が見つかりません".into());
        };
        slot.binding.assignment = None;
        slot.binding.pinned = false;
        Ok(())
    }

    /// Keep every sender registered while its assigned feed is quiet.
    pub fn hold_open(&mut self) {
        let stale = Duration::from_millis(700);
        for slot in &mut self.slots {
            if slot.last_output.elapsed() < stale {
                continue;
            }
            let audio = slot.kind != SourceKind::Camera;
            let video = slot.kind != SourceKind::Audio;
            if audio {
                let _ = slot.publisher.publish_audio(&self.silence, 48_000, 2);
            }
            if video {
                let _ = slot.publisher.publish_video(&self.black, 640, 360, (1, 1));
            }
            slot.last_output = Instant::now();
        }
    }

    pub fn snapshots(&mut self, idle_after: Duration) -> Vec<SlotSnapshot> {
        self.slots
            .iter_mut()
            .map(|slot| {
                slot.refresh_fps();
                let (channel_id, user_id) = slot
                    .binding
                    .assignment
                    .map(|feed| (Some(feed.channel_id), Some(feed.user_id)))
                    .unwrap_or((None, None));
                let assigned_name = user_id
                    .and_then(|id| self.names.get(&id).cloned())
                    .unwrap_or_default();
                let live = slot
                    .last_media
                    .map(|instant| instant.elapsed() < idle_after)
                    .unwrap_or(false);
                SlotSnapshot {
                    id: slot.id.clone(),
                    kind: slot.kind.label().to_owned(),
                    name: slot.name.clone(),
                    channel_id,
                    user_id,
                    assigned_name,
                    pinned: slot.binding.pinned,
                    live,
                    frames: slot.frames,
                    dropped: slot.dropped,
                    fps: slot.fps,
                }
            })
            .collect()
    }

    pub fn preview_frames(&mut self) -> Vec<PreviewFrame> {
        self.previews.retain(|slot| slot.at.elapsed() < Duration::from_secs(5));
        self.previews
            .iter()
            .map(|slot| PreviewFrame {
                user_id: slot.user_id.to_string(),
                name: self
                    .names
                    .get(&slot.user_id)
                    .cloned()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "取得中".into()),
                kind: slot.kind.slug().to_owned(),
                width: slot.width,
                height: slot.height,
                rgba: encode_base64(&slot.rgba),
            })
            .collect()
    }

    fn remember_preview(&mut self, user_id: u64, kind: SourceKind, bgra: &[u8], width: u32, height: u32) {
        if let Some(slot) = self.previews.iter().find(|slot| slot.user_id == user_id && slot.kind == kind) {
            if slot.at.elapsed() < Duration::from_millis(200) {
                return;
            }
        }
        let (width, height, rgba) = downscale_bgra_to_rgba(bgra, width, height, 960);
        if rgba.is_empty() {
            return;
        }
        if let Some(slot) = self.previews.iter_mut().find(|slot| slot.user_id == user_id && slot.kind == kind) {
            slot.width = width;
            slot.height = height;
            slot.rgba = rgba;
            slot.at = Instant::now();
        } else {
            self.previews.push(PreviewSlot {
                user_id,
                kind,
                width,
                height,
                rgba,
                at: Instant::now(),
            });
        }
    }

    fn indexes_for(&self, feed: FeedKey) -> Vec<usize> {
        let bindings: Vec<Binding> = self.slots.iter().map(|slot| slot.binding).collect();
        routed(&bindings, feed)
    }

    fn reopen(&mut self, index: usize, kind: SourceKind, transport: Transport) -> Result<(), String> {
        let name = self.slots[index].name.clone();
        let publisher = open_publisher(&name, kind, transport)?;
        let slot = &mut self.slots[index];
        slot.kind = kind;
        slot.binding.kind = kind;
        slot.transport = transport;
        slot.publisher = publisher;
        slot.last_output = Instant::now().checked_sub(Duration::from_secs(2)).unwrap_or_else(Instant::now);
        Ok(())
    }
}

fn open_publisher(name: &str, kind: SourceKind, transport: Transport) -> Result<FanoutPublisher, String> {
    let (omt, ndi) = transport.flags();
    if ndi && !cfg!(feature = "ndi") {
        return Err("NDI を使うには NDI 6 SDK を入れ、ndi 機能付きでビルドしてください".into());
    }
    FanoutPublisher::open(name, kind.mode(), omt, ndi).map_err(|err| err.to_string())
}

impl OutputSlot {
    fn note_media(&mut self) {
        self.frames += 1;
        self.window_frames += 1;
        let now = Instant::now();
        self.last_media = Some(now);
        self.last_output = now;
        self.refresh_fps();
    }

    fn refresh_fps(&mut self) {
        let elapsed = self.window_start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            self.fps = self.window_frames as f64 / elapsed;
            self.window_frames = 0;
            self.window_start = Instant::now();
        }
    }
}

fn route_feed(kind: SourceKind, channel_id: u64, user_id: u64) -> Option<FeedKey> {
    if channel_id == 0 || user_id == 0 {
        None
    } else {
        Some(FeedKey { channel_id, user_id, kind })
    }
}

fn routed(slots: &[Binding], feed: FeedKey) -> Vec<usize> {
    slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.kind == feed.kind && slot.assignment == Some(feed))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
fn pin(slots: &mut [Binding], index: usize, channel_id: u64, user_id: u64) {
    let kind = slots[index].kind;
    slots[index].assignment = Some(FeedKey { channel_id, user_id, kind });
    slots[index].pinned = true;
}

pub fn log_sources(snapshots: &[SlotSnapshot]) {
    if snapshots.is_empty() {
        tracing::info!("no outputs");
        return;
    }
    for source in snapshots {
        tracing::info!(
            name = %source.name,
            assigned = %source.assigned_name,
            live = source.live,
            frames = source.frames,
            dropped = source.dropped,
            fps = format!("{:.1}", source.fps),
            "output"
        );
    }
}

fn downscale_bgra_to_rgba(bgra: &[u8], width: u32, height: u32, max_width: u32) -> (u32, u32, Vec<u8>) {
    let pixels = (width as usize).saturating_mul(height as usize).saturating_mul(4);
    if width == 0 || height == 0 || bgra.len() < pixels {
        return (0, 0, Vec::new());
    }
    let out_w = width.min(max_width).max(1);
    let out_h = ((height as u64 * out_w as u64) / width as u64).max(1) as u32;
    let mut rgba = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    for y in 0..out_h {
        let src_y = (y as u64 * height as u64 / out_h as u64) as u32;
        for x in 0..out_w {
            let src_x = (x as u64 * width as u64 / out_w as u64) as u32;
            let source = ((src_y * width + src_x) * 4) as usize;
            let dest = ((y * out_w + x) * 4) as usize;
            rgba[dest] = bgra[source + 2];
            rgba[dest + 1] = bgra[source + 1];
            rgba[dest + 2] = bgra[source];
            rgba[dest + 3] = 255;
        }
    }
    (out_w, out_h, rgba)
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(kind: SourceKind) -> Binding {
        Binding { kind, assignment: None, pinned: false }
    }

    #[test]
    fn explicit_route_stays_when_the_feed_returns() {
        let mut slots = vec![empty(SourceKind::Camera), empty(SourceKind::Camera)];
        let alice = FeedKey { channel_id: 7, user_id: 1, kind: SourceKind::Camera };
        pin(&mut slots, 0, 7, 1);
        assert_eq!(routed(&slots, alice), vec![0]);
        assert_eq!(routed(&slots, alice), vec![0]);
    }

    #[test]
    fn unrouted_feed_does_not_occupy_an_output() {
        let slots = vec![empty(SourceKind::Audio)];
        let feed = FeedKey { channel_id: 1, user_id: 9, kind: SourceKind::Audio };
        assert!(routed(&slots, feed).is_empty());
    }

    #[test]
    fn the_same_feed_can_route_to_two_outputs() {
        let mut slots = vec![empty(SourceKind::Camera), empty(SourceKind::Camera)];
        pin(&mut slots, 0, 7, 1);
        pin(&mut slots, 1, 7, 1);
        let alice = FeedKey { channel_id: 7, user_id: 1, kind: SourceKind::Camera };
        assert_eq!(routed(&slots, alice), vec![0, 1]);
    }

    #[test]
    fn transport_parse_accepts_omt_and_ndi() {
        assert_eq!(Transport::parse("omt").unwrap(), Transport::Omt);
        assert_eq!(Transport::parse("NDI").unwrap(), Transport::Ndi);
        assert!(Transport::parse("both").is_err());
    }

    #[test]
    fn audio_output_ignores_a_camera_feed() {
        let mut slots = vec![empty(SourceKind::Audio)];
        pin(&mut slots, 0, 1, 4);
        let camera = FeedKey { channel_id: 1, user_id: 4, kind: SourceKind::Camera };
        assert!(routed(&slots, camera).is_empty());
        let audio = FeedKey { channel_id: 1, user_id: 4, kind: SourceKind::Audio };
        assert_eq!(routed(&slots, audio), vec![0]);
    }
}
