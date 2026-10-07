use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelTarget {
    #[serde(deserialize_with = "snowflake")]
    pub guild_id: u64,
    #[serde(deserialize_with = "snowflake")]
    pub channel_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default, deserialize_with = "snowflake")]
    pub guild_id: u64,
    #[serde(default, deserialize_with = "snowflake")]
    pub channel_id: u64,
    #[serde(default)]
    pub channels: Vec<ChannelTarget>,
    #[serde(default = "default_true")]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default = "default_true")]
    pub enable_omt: bool,
    #[serde(default)]
    pub enable_ndi: bool,
    #[serde(default = "default_prefix")]
    pub source_prefix: String,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub allow_user_ids: Vec<u64>,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub deny_user_ids: Vec<u64>,
    #[serde(default = "default_streams")]
    pub max_streams: usize,
    #[serde(default = "default_slots")]
    pub max_video_slots: usize,
    #[serde(default = "default_quality")]
    pub prefer_quality: u32,
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_audio_slots")]
    pub audio_slots: usize,
    #[serde(default = "default_camera_slots")]
    pub camera_slots: usize,
    #[serde(default = "default_golive_slots")]
    pub golive_slots: usize,
    #[serde(default)]
    pub client_build_number: u32,
}

fn default_true() -> bool {
    true
}
fn default_prefix() -> String {
    "Discord".into()
}
fn default_streams() -> usize {
    8
}
fn default_slots() -> usize {
    8
}
fn default_quality() -> u32 {
    100
}
fn default_idle() -> u64 {
    5
}
fn default_audio_slots() -> usize {
    8
}
fn default_camera_slots() -> usize {
    8
}
fn default_golive_slots() -> usize {
    4
}

fn snowflake<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    parse_snowflake(Snowflake::deserialize(deserializer)?)
}

fn snowflake_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u64>, D::Error> {
    Vec::<Snowflake>::deserialize(deserializer)?
        .into_iter()
        .map(parse_snowflake)
        .collect()
}

fn parse_snowflake<E: serde::de::Error>(value: Snowflake) -> Result<u64, E> {
    match value {
        Snowflake::Int(value) => Ok(value),
        Snowflake::Str(value) => value.parse().map_err(E::custom),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Snowflake {
    Int(u64),
    Str(String),
}

pub fn default_settings_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("discord-omt-bridge").join("settings.toml")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&text)?;
        config.audio_slots = config.audio_slots.min(32);
        config.camera_slots = config.camera_slots.min(32);
        config.golive_slots = config.golive_slots.min(32);
        if !config.enable_omt && !config.enable_ndi {
            anyhow::bail!("enable_omt か enable_ndi の少なくとも一方を true にしてください");
        }
        if config.enable_ndi && !cfg!(feature = "ndi") {
            anyhow::bail!("NDI を使うには `cargo build -p discord-omt-bridge --features ndi` と NDI 6 SDK が必要です");
        }
        Ok(config)
    }

    pub fn channel_targets(&self) -> Vec<(u64, u64)> {
        let mut targets = Vec::new();
        if self.guild_id != 0 && self.channel_id != 0 {
            targets.push((self.guild_id, self.channel_id));
        }
        for channel in &self.channels {
            let target = (channel.guild_id, channel.channel_id);
            if target.0 != 0 && target.1 != 0 && !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    pub fn allows(&self, user_id: u64) -> bool {
        if self.deny_user_ids.contains(&user_id) {
            return false;
        }
        self.allow_user_ids.is_empty() || self.allow_user_ids.contains(&user_id)
    }

    pub fn apply_runtime(&mut self, runtime: &RuntimeSettings) {
        self.self_mute = runtime.self_mute;
        self.self_deaf = runtime.self_deaf;
        self.enable_omt = runtime.enable_omt;
        self.enable_ndi = runtime.enable_ndi;
        self.source_prefix = runtime.source_prefix.clone();
        self.allow_user_ids = runtime.allow_user_ids.clone();
        self.deny_user_ids = runtime.deny_user_ids.clone();
        self.max_streams = runtime.max_streams;
        self.max_video_slots = runtime.max_video_slots;
        self.prefer_quality = runtime.prefer_quality;
        self.idle_timeout_secs = runtime.idle_timeout_secs;
    }
}

/// Tunables that can change while the bridge is running. The token stays out of this struct.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub self_mute: bool,
    pub self_deaf: bool,
    pub enable_omt: bool,
    pub enable_ndi: bool,
    pub source_prefix: String,
    pub allow_user_ids: Vec<u64>,
    pub deny_user_ids: Vec<u64>,
    pub max_streams: usize,
    pub max_video_slots: usize,
    pub prefer_quality: u32,
    pub idle_timeout_secs: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub token: String,
    #[serde(default = "default_true")]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default = "default_true")]
    pub enable_omt: bool,
    #[serde(default)]
    pub enable_ndi: bool,
    #[serde(default = "default_prefix")]
    pub source_prefix: String,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub allow_user_ids: Vec<u64>,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub deny_user_ids: Vec<u64>,
    #[serde(default = "default_streams")]
    pub max_streams: usize,
    #[serde(default = "default_slots")]
    pub max_video_slots: usize,
    #[serde(default = "default_quality")]
    pub prefer_quality: u32,
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_audio_slots")]
    pub audio_slots: usize,
    #[serde(default = "default_camera_slots")]
    pub camera_slots: usize,
    #[serde(default = "default_golive_slots")]
    pub golive_slots: usize,
    #[serde(default)]
    pub client_build_number: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            token: String::new(),
            self_mute: true,
            self_deaf: false,
            enable_omt: true,
            enable_ndi: false,
            source_prefix: default_prefix(),
            allow_user_ids: Vec::new(),
            deny_user_ids: Vec::new(),
            max_streams: default_streams(),
            max_video_slots: default_slots(),
            prefer_quality: default_quality(),
            idle_timeout_secs: default_idle(),
            audio_slots: default_audio_slots(),
            camera_slots: default_camera_slots(),
            golive_slots: default_golive_slots(),
            client_build_number: 0,
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("token", &"<redacted>")
            .field("self_mute", &self.self_mute)
            .field("enable_omt", &self.enable_omt)
            .field("enable_ndi", &self.enable_ndi)
            .finish_non_exhaustive()
    }
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn runtime(&self) -> RuntimeSettings {
        RuntimeSettings {
            self_mute: self.self_mute,
            self_deaf: self.self_deaf,
            enable_omt: self.enable_omt,
            enable_ndi: self.enable_ndi,
            source_prefix: self.source_prefix.clone(),
            allow_user_ids: self.allow_user_ids.clone(),
            deny_user_ids: self.deny_user_ids.clone(),
            max_streams: self.max_streams.max(1),
            max_video_slots: self.max_video_slots.clamp(1, 32),
            prefer_quality: self.prefer_quality.min(100),
            idle_timeout_secs: self.idle_timeout_secs.max(1),
        }
    }

    pub fn to_config(&self) -> anyhow::Result<Config> {
        let runtime = self.runtime();
        if !runtime.enable_omt && !runtime.enable_ndi {
            anyhow::bail!("OMT か NDI の少なくとも一方を有効にしてください");
        }
        if runtime.enable_ndi && !crate::ndi_compiled() {
            anyhow::bail!("NDI を使うには NDI 6 SDK を入れ、`--features ndi` でビルドしてください");
        }
        if runtime.source_prefix.trim().is_empty() {
            anyhow::bail!("ソース名の接頭辞を入力してください");
        }
        Ok(Config {
            guild_id: 0,
            channel_id: 0,
            channels: Vec::new(),
            self_mute: runtime.self_mute,
            self_deaf: runtime.self_deaf,
            enable_omt: runtime.enable_omt,
            enable_ndi: runtime.enable_ndi,
            source_prefix: runtime.source_prefix,
            allow_user_ids: runtime.allow_user_ids,
            deny_user_ids: runtime.deny_user_ids,
            max_streams: runtime.max_streams,
            max_video_slots: runtime.max_video_slots,
            prefer_quality: runtime.prefer_quality,
            idle_timeout_secs: runtime.idle_timeout_secs,
            audio_slots: self.audio_slots.clamp(0, 32),
            camera_slots: self.camera_slots.clamp(0, 32),
            golive_slots: self.golive_slots.clamp(0, 32),
            client_build_number: self.client_build_number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_overrides_allow() {
        let config = Config {
            guild_id: 1,
            channel_id: 2,
            channels: Vec::new(),
            self_mute: true,
            self_deaf: false,
            enable_omt: true,
            enable_ndi: false,
            source_prefix: "Discord".into(),
            allow_user_ids: vec![5],
            deny_user_ids: vec![5],
            max_streams: 8,
            max_video_slots: 8,
            prefer_quality: 100,
            idle_timeout_secs: 5,
            audio_slots: 8,
            camera_slots: 8,
            golive_slots: 4,
            client_build_number: 0,
        };
        assert!(!config.allows(5));
        assert!(!config.allows(9));
        assert_eq!(config.channel_targets(), vec![(1, 2)]);
    }

    #[test]
    fn string_snowflakes_and_extra_channels_parse() {
        let config: Config = toml::from_str(
            r#"
            guild_id = "10"
            channel_id = "20"
            [[channels]]
            guild_id = "10"
            channel_id = "21"
            "#,
        )
        .unwrap();
        assert_eq!(config.channel_targets(), vec![(10, 20), (10, 21)]);
    }
}
