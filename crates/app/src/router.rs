use std::collections::HashMap;
use std::time::{Duration, Instant};

use egress::{AudioPublisher, FanoutPublisher, VideoPublisher};

use crate::config::Config;

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

    fn slug(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Camera => "camera",
            Self::GoLive => "golive",
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

pub struct SourceRouter {
    slots: Vec<OutputSlot>,
    names: HashMap<u64, String>,
    black: Vec<u8>,
    silence: Vec<i16>,
    overflow: u64,
}

impl SourceRouter {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let mut router = Self {
            slots: Vec::new(),
            names: HashMap::new(),
            black: vec![0; 640 * 360 * 4],
            silence: vec![0; 960 * 2],
            overflow: 0,
        };
        router.add_kind(config, SourceKind::Audio, config.audio_slots)?;
        router.add_kind(config, SourceKind::Camera, config.camera_slots)?;
        router.add_kind(config, SourceKind::GoLive, config.golive_slots)?;
        tracing::info!(slots = router.slots.len(), "opened persistent LAN slots");
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
        let Some(index) = claim_slot(&mut self.slots, feed) else {
            self.overflow += 1;
            return;
        };
        let slot = &mut self.slots[index];
        if let Err(err) = slot.publisher.publish_audio(pcm, sample_rate, channels) {
            slot.dropped += 1;
            tracing::warn!(%err, slot = %slot.name, "audio publish failed");
        } else {
            slot.note_media();
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
        let feed = FeedKey { channel_id, user_id, kind };
        let Some(index) = claim_slot(&mut self.slots, feed) else {
            self.overflow += 1;
            return;
        };
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

    pub fn assign(&mut self, slot_id: &str, channel_id: u64, user_id: u64) -> Result<(), String> {
        let Some(index) = self.slots.iter().position(|slot| slot.id == slot_id) else {
            return Err("スロットが見つかりません".into());
        };
        let kind = self.slots[index].kind;
        let feed = FeedKey { channel_id, user_id, kind };
        for (other, slot) in self.slots.iter_mut().enumerate() {
            if other != index && slot.binding.assignment == Some(feed) {
                slot.binding.assignment = None;
                slot.binding.pinned = false;
            }
        }
        self.slots[index].binding.assignment = Some(feed);
        self.slots[index].binding.pinned = true;
        Ok(())
    }

    pub fn clear(&mut self, slot_id: &str) -> Result<(), String> {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == slot_id) else {
            return Err("スロットが見つかりません".into());
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

    fn add_kind(&mut self, config: &Config, kind: SourceKind, count: usize) -> anyhow::Result<()> {
        for index in 1..=count {
            let name = format!("{} / {} / {index}", config.source_prefix, kind.label());
            let publisher = FanoutPublisher::open(&name, kind.mode(), config.enable_omt, config.enable_ndi)
                .map_err(|err| anyhow::anyhow!(err))?;
            tracing::info!(%name, "created output slot");
            self.slots.push(OutputSlot {
                id: format!("{}-{index}", kind.slug()),
                kind,
                name,
                publisher,
                binding: Binding { kind, assignment: None, pinned: false },
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
        }
        Ok(())
    }
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

fn claim_slot(slots: &mut [OutputSlot], feed: FeedKey) -> Option<usize> {
    let mut bindings: Vec<Binding> = slots.iter().map(|slot| slot.binding).collect();
    let index = claim(&mut bindings, feed)?;
    for (slot, binding) in slots.iter_mut().zip(bindings) {
        slot.binding = binding;
    }
    Some(index)
}

fn claim(slots: &mut [Binding], feed: FeedKey) -> Option<usize> {
    if let Some(index) = slots.iter().position(|slot| slot.assignment == Some(feed)) {
        return Some(index);
    }
    let index = slots.iter().position(|slot| slot.kind == feed.kind && slot.assignment.is_none())?;
    slots[index].assignment = Some(feed);
    slots[index].pinned = false;
    Some(index)
}

pub fn log_sources(snapshots: &[SlotSnapshot]) {
    if snapshots.is_empty() {
        tracing::info!("no output slots");
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
            "slot"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(kind: SourceKind) -> Binding {
        Binding { kind, assignment: None, pinned: false }
    }

    #[test]
    fn assignment_stays_when_the_feed_returns() {
        let mut slots = vec![empty(SourceKind::Camera), empty(SourceKind::Camera)];
        let alice = FeedKey { channel_id: 7, user_id: 1, kind: SourceKind::Camera };
        assert_eq!(claim(&mut slots, alice), Some(0));
        assert_eq!(claim(&mut slots, alice), Some(0));
        let bob = FeedKey { channel_id: 7, user_id: 2, kind: SourceKind::Camera };
        assert_eq!(claim(&mut slots, bob), Some(1));
        assert_eq!(claim(&mut slots, alice), Some(0));
    }

    #[test]
    fn a_full_pool_does_not_steal_a_quiet_slot() {
        let mut slots = vec![empty(SourceKind::Audio)];
        let first = FeedKey { channel_id: 1, user_id: 9, kind: SourceKind::Audio };
        let second = FeedKey { channel_id: 1, user_id: 8, kind: SourceKind::Audio };
        assert_eq!(claim(&mut slots, first), Some(0));
        assert_eq!(claim(&mut slots, second), None);
        assert_eq!(slots[0].assignment, Some(first));
    }

    #[test]
    fn audio_and_camera_use_different_pools() {
        let mut slots = vec![empty(SourceKind::Audio), empty(SourceKind::Camera)];
        let camera = FeedKey { channel_id: 1, user_id: 4, kind: SourceKind::Camera };
        assert_eq!(claim(&mut slots, camera), Some(1));
    }
}
