mod bridge;
mod config;
mod router;
mod snapshot;

pub use bridge::{run, spawn_session, BridgeHandle};
pub use config::{default_settings_path, Config, RuntimeSettings, Settings};
pub use snapshot::Snapshot;

pub fn ndi_compiled() -> bool {
    cfg!(feature = "ndi")
}
