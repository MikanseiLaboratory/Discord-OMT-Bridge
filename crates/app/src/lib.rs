mod bridge;
mod config;
mod router;
mod snapshot;

pub use bridge::{run, spawn_session, BridgeHandle};
pub use discord_media::maybe_run_av1_worker;
pub use discord_user_gateway::{
    login_password, send_login_sms, submit_mfa, AuthStep, CaptchaAnswer, CaptchaChallenge,
};
pub use config::{default_settings_path, AudioSource, Config, OutputRoute, RuntimeSettings, Settings};
pub use egress::{list_output_devices, ListedDevice};
pub use router::{SourceKind, Transport};
pub use snapshot::{PreviewFrame, Snapshot};

pub fn ndi_compiled() -> bool {
    cfg!(feature = "ndi")
}

pub fn asio_compiled() -> bool {
    egress::asio_available()
}
