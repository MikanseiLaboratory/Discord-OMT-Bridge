use serde_json::Value;

use crate::client::parse_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Voice,
    Category,
    Stage,
    Other,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
            Self::Category => "category",
            Self::Stage => "stage",
            Self::Other => "other",
        }
    }

    fn from_discord(value: i64) -> Self {
        match value {
            0 | 5 => Self::Text,
            2 => Self::Voice,
            4 => Self::Category,
            13 => Self::Stage,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelRecord {
    pub id: u64,
    pub name: String,
    pub kind: ChannelKind,
    pub position: i64,
    pub parent_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct GuildRecord {
    pub id: u64,
    pub name: String,
    pub icon: Option<String>,
    /// `None` when the payload did not include a channel list, such as a name-only guild update.
    pub channels: Option<Vec<ChannelRecord>>,
}

pub fn read_guild(data: &Value) -> Option<GuildRecord> {
    let id = parse_id(data.get("id"))?;
    let name = data
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("サーバー")
        .to_owned();
    let icon = data
        .get("icon")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let channels = data.get("channels").and_then(|value| value.as_array()).map(|channels| {
        channels.iter().filter_map(read_channel).collect()
    });
    Some(GuildRecord {
        id,
        name,
        icon,
        channels,
    })
}

pub fn read_channel(data: &Value) -> Option<ChannelRecord> {
    let id = parse_id(data.get("id"))?;
    let name = data
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("channel")
        .to_owned();
    let kind = ChannelKind::from_discord(data.get("type").and_then(|value| value.as_i64()).unwrap_or(-1));
    let position = data.get("position").and_then(|value| value.as_i64()).unwrap_or(0);
    let parent_id = parse_id(data.get("parent_id"));
    Some(ChannelRecord {
        id,
        name,
        kind,
        position,
        parent_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_guild_channels_and_keeps_partial_updates() {
        let created = json!({
            "id": "100",
            "name": "Studio",
            "icon": "abc",
            "channels": [
                {"id": "1", "name": "talk", "type": 4, "position": 0},
                {"id": "2", "name": "booth", "type": 2, "position": 1, "parent_id": "1"},
                {"id": "3", "name": "notes", "type": 0, "position": 2, "parent_id": "1"}
            ]
        });
        let guild = read_guild(&created).unwrap();
        let channels = guild.channels.unwrap();
        assert_eq!(guild.name, "Studio");
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[1].kind, ChannelKind::Voice);
        assert_eq!(channels[1].parent_id, Some(1));

        let renamed = json!({"id": "100", "name": "Studio 2", "icon": null});
        let update = read_guild(&renamed).unwrap();
        assert!(update.channels.is_none());
        assert_eq!(update.name, "Studio 2");
    }
}
