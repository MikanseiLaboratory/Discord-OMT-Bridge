mod av1_ipc;
mod av1_worker;
mod connection;
mod dave;
mod decode;
mod jitter;
mod sdp;

pub use av1_ipc::{Av1IpcError, Av1Supervisor, FLAG_ABORT, FLAG_BAD_ID, FLAG_HANG};
pub use av1_worker::maybe_run_av1_worker;
pub use connection::{spawn_media, ConnectParams, MediaEvent, MediaHandle, MediaKind, VideoPicture};
pub use sdp::{advertisements_from_offer, select_protocol_fragment, synthesize_answer};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("voice: {0}")]
    Voice(String),
    #[error("dave: {0}")]
    Dave(String),
    #[error("webrtc: {0}")]
    WebRtc(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Decode(#[from] decode::DecodeError),
}
