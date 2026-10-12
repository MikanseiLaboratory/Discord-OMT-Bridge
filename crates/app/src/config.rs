use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRoute {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, deserialize_with = "snowflake")]
    pub channel_id: u64,
    #[serde(default, deserialize_with = "snowflake")]
    pub user_id: u64,
    /// `omt`, `ndi`, `device`, or `asio`.
    #[serde(default = "default_output_transport")]
    pub transport: String,
    #[serde(default)]
    pub device: String,
    /// 1-based first ASIO channel for this output.
    #[serde(default = "default_channel_start")]
    pub channel_start: u16,
    /// How many ASIO channels this output writes. Stereo uses 2.
    #[serde(default = "default_channel_count")]
    pub channel_count: u16,
    /// People mixed into an audio output, each with their own volume.
    #[serde(default)]
    pub members: Vec<AudioSource>,
}

fn default_output_transport() -> String {
    "omt".into()
}

fn default_channel_start() -> u16 {
    1
}

fn default_channel_count() -> u16 {
    2
}

fn default_volume() -> u16 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSource {
    #[serde(default, deserialize_with = "snowflake")]
    pub channel_id: u64,
    #[serde(default, deserialize_with = "snowflake")]
    pub user_id: u64,
    /// 0–200. 100 matches Discord's default user volume.
    #[serde(default = "default_volume")]
    pub volume: u16,
}

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
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub allow_user_ids: Vec<u64>,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub deny_user_ids: Vec<u64>,
    #[serde(default = "default_slots")]
    pub max_video_slots: usize,
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    #[serde(default)]
    pub client_build_number: u32,
    #[serde(default)]
    pub outputs: Vec<OutputRoute>,
}

fn default_slots() -> usize {
    8
}
fn default_idle() -> u64 {
    5
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
    PathBuf::from(base)
        .join("discord-omt-bridge")
        .join("settings.toml")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&text)?;
        config.max_video_slots = config.max_video_slots.clamp(1, 32);
        config.idle_timeout_secs = config.idle_timeout_secs.max(1);
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
        self.self_deaf = runtime.self_deaf;
        self.allow_user_ids = runtime.allow_user_ids.clone();
        self.deny_user_ids = runtime.deny_user_ids.clone();
        self.max_video_slots = runtime.max_video_slots;
        self.idle_timeout_secs = runtime.idle_timeout_secs;
    }
}

/// Tunables that can change while the bridge is running. The token stays out of this struct.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub self_deaf: bool,
    pub allow_user_ids: Vec<u64>,
    pub deny_user_ids: Vec<u64>,
    pub max_video_slots: usize,
    pub idle_timeout_secs: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub allow_user_ids: Vec<u64>,
    #[serde(default, deserialize_with = "snowflake_list")]
    pub deny_user_ids: Vec<u64>,
    #[serde(default = "default_slots")]
    pub max_video_slots: usize,
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    #[serde(default)]
    pub client_build_number: u32,
    #[serde(default)]
    pub outputs: Vec<OutputRoute>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            token: String::new(),
            self_deaf: false,
            allow_user_ids: Vec::new(),
            deny_user_ids: Vec::new(),
            max_video_slots: default_slots(),
            idle_timeout_secs: default_idle(),
            client_build_number: 0,
            outputs: Vec::new(),
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("token", &"<redacted>")
            .field("self_deaf", &self.self_deaf)
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
            self_deaf: self.self_deaf,
            allow_user_ids: self.allow_user_ids.clone(),
            deny_user_ids: self.deny_user_ids.clone(),
            max_video_slots: self.max_video_slots.clamp(1, 32),
            idle_timeout_secs: self.idle_timeout_secs.max(1),
        }
    }

    pub fn to_config(&self) -> anyhow::Result<Config> {
        let runtime = self.runtime();
        Ok(Config {
            guild_id: 0,
            channel_id: 0,
            channels: Vec::new(),
            self_deaf: runtime.self_deaf,
            allow_user_ids: runtime.allow_user_ids,
            deny_user_ids: runtime.deny_user_ids,
            max_video_slots: runtime.max_video_slots,
            idle_timeout_secs: runtime.idle_timeout_secs,
            client_build_number: self.client_build_number,
            outputs: self.outputs.clone(),
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
            self_deaf: false,
            allow_user_ids: vec![5],
            deny_user_ids: vec![5],
            max_video_slots: 8,
            idle_timeout_secs: 5,
            client_build_number: 0,
            outputs: Vec::new(),
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
        assert!(config.outputs.is_empty());
    }
}
