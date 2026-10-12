mod client;
mod directory;
mod login;

pub use client::{
    spawn_gateway, GatewayCommand, GatewayEvent, GatewayHandle, StreamEndpoint, VoiceEndpoint,
    VoiceMember,
};
pub use directory::{ChannelKind, ChannelRecord, GuildRecord};
pub use login::{
    login_password, send_login_sms, submit_mfa, AuthStep, CaptchaAnswer, CaptchaChallenge,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("discord gateway: {0}")]
    Message(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn stream_key(guild_id: u64, channel_id: u64, user_id: u64) -> String {
    format!("guild:{guild_id}:{channel_id}:{user_id}")
}

pub fn parse_stream_key(key: &str) -> Option<(u64, u64, u64)> {
    let mut parts = key.split(':');
    let kind = parts.next()?;
    if kind != "guild" {
        return None;
    }
    let guild_id = parts.next()?.parse().ok()?;
    let channel_id = parts.next()?.parse().ok()?;
    let user_id = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((guild_id, channel_id, user_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_roundtrip() {
        let key = stream_key(1, 2, 3);
        assert_eq!(key, "guild:1:2:3");
        assert_eq!(parse_stream_key(&key), Some((1, 2, 3)));
        assert_eq!(parse_stream_key("call:1:2"), None);
    }
}
