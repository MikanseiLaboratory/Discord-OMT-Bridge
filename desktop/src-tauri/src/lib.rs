use std::path::PathBuf;
use std::sync::Mutex;

use discord_omt_bridge::{default_settings_path, spawn_session, BridgeHandle, Settings, Snapshot};
use serde::{Deserialize, Serialize};
use tauri::{Manager, RunEvent, State};

struct AppState {
    bridge: Mutex<Option<BridgeHandle>>,
    settings_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsDto {
    token: String,
    self_mute: bool,
    self_deaf: bool,
    enable_omt: bool,
    enable_ndi: bool,
    source_prefix: String,
    allow_user_ids: Vec<String>,
    deny_user_ids: Vec<String>,
    max_streams: u32,
    max_video_slots: u32,
    prefer_quality: u32,
    idle_timeout_secs: u64,
    audio_slots: u32,
    camera_slots: u32,
    golive_slots: u32,
    client_build_number: u32,
    path: String,
}

fn parse_ids(values: &[String]) -> Result<Vec<u64>, String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("ユーザー ID が不正です: {value}"))
        })
        .collect()
}

fn to_dto(settings: &Settings, path: &PathBuf) -> SettingsDto {
    SettingsDto {
        token: settings.token.clone(),
        self_mute: settings.self_mute,
        self_deaf: settings.self_deaf,
        enable_omt: settings.enable_omt,
        enable_ndi: settings.enable_ndi,
        source_prefix: settings.source_prefix.clone(),
        allow_user_ids: settings.allow_user_ids.iter().map(|id| id.to_string()).collect(),
        deny_user_ids: settings.deny_user_ids.iter().map(|id| id.to_string()).collect(),
        max_streams: settings.max_streams as u32,
        max_video_slots: settings.max_video_slots as u32,
        prefer_quality: settings.prefer_quality,
        idle_timeout_secs: settings.idle_timeout_secs,
        audio_slots: settings.audio_slots as u32,
        camera_slots: settings.camera_slots as u32,
        golive_slots: settings.golive_slots as u32,
        client_build_number: settings.client_build_number,
        path: path.display().to_string(),
    }
}

fn from_dto(dto: &SettingsDto) -> Result<Settings, String> {
    Ok(Settings {
        token: dto.token.trim().to_owned(),
        self_mute: dto.self_mute,
        self_deaf: dto.self_deaf,
        enable_omt: dto.enable_omt,
        enable_ndi: dto.enable_ndi,
        source_prefix: dto.source_prefix.trim().to_owned(),
        allow_user_ids: parse_ids(&dto.allow_user_ids)?,
        deny_user_ids: parse_ids(&dto.deny_user_ids)?,
        max_streams: dto.max_streams.max(1) as usize,
        max_video_slots: dto.max_video_slots.clamp(1, 32) as usize,
        prefer_quality: dto.prefer_quality.min(100),
        idle_timeout_secs: dto.idle_timeout_secs.max(1),
        audio_slots: dto.audio_slots.min(32) as usize,
        camera_slots: dto.camera_slots.min(32) as usize,
        golive_slots: dto.golive_slots.min(32) as usize,
        client_build_number: dto.client_build_number,
    })
}

fn stop_bridge(state: &AppState) {
    let running = state.bridge.lock().expect("bridge lock").take();
    if let Some(handle) = running {
        handle.shutdown_and_join();
    }
}

#[tauri::command]
fn load_settings(state: State<'_, AppState>) -> Result<SettingsDto, String> {
    let settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    Ok(to_dto(&settings, &state.settings_path))
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: SettingsDto) -> Result<(), String> {
    let settings = from_dto(&settings)?;
    settings.to_config().map_err(|err| err.to_string())?;
    settings.save(&state.settings_path).map_err(|err| err.to_string())?;
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.apply(settings.runtime())?;
    }
    Ok(())
}

#[tauri::command]
fn connect(state: State<'_, AppState>) -> Result<(), String> {
    let settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    if settings.token.trim().is_empty() {
        return Err("トークンを入力してください".into());
    }
    let config = settings.to_config().map_err(|err| err.to_string())?;
    stop_bridge(&state);
    let handle = spawn_session(settings.token.trim().to_owned(), config);
    *state.bridge.lock().expect("bridge lock") = Some(handle);
    Ok(())
}

#[tauri::command]
fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    stop_bridge(&state);
    Ok(())
}

#[tauri::command]
fn join_voice(state: State<'_, AppState>, guild_id: String, channel_id: String) -> Result<(), String> {
    let guild_id = guild_id.parse::<u64>().map_err(|_| "サーバー ID が不正です".to_owned())?;
    let channel_id = channel_id.parse::<u64>().map_err(|_| "チャンネル ID が不正です".to_owned())?;
    let guard = state.bridge.lock().expect("bridge lock");
    let Some(handle) = guard.as_ref() else {
        return Err("先に接続してください".into());
    };
    handle.join(guild_id, channel_id)
}

#[tauri::command]
fn leave_voice(state: State<'_, AppState>, guild_id: String, channel_id: String) -> Result<(), String> {
    let guild_id = guild_id.parse::<u64>().map_err(|_| "サーバー ID が不正です".to_owned())?;
    let channel_id = channel_id.parse::<u64>().map_err(|_| "チャンネル ID が不正です".to_owned())?;
    let guard = state.bridge.lock().expect("bridge lock");
    let Some(handle) = guard.as_ref() else {
        return Err("接続していません".into());
    };
    handle.leave_channel(guild_id, channel_id)
}

#[tauri::command]
fn assign_slot(
    state: State<'_, AppState>,
    slot_id: String,
    channel_id: String,
    user_id: String,
) -> Result<(), String> {
    let channel_id = channel_id.parse::<u64>().map_err(|_| "チャンネル ID が不正です".to_owned())?;
    let user_id = user_id.parse::<u64>().map_err(|_| "ユーザー ID が不正です".to_owned())?;
    let guard = state.bridge.lock().expect("bridge lock");
    let Some(handle) = guard.as_ref() else {
        return Err("先に接続してください".into());
    };
    handle.assign_slot(slot_id, channel_id, user_id)
}

#[tauri::command]
fn clear_slot(state: State<'_, AppState>, slot_id: String) -> Result<(), String> {
    let guard = state.bridge.lock().expect("bridge lock");
    let Some(handle) = guard.as_ref() else {
        return Err("先に接続してください".into());
    };
    handle.clear_slot(slot_id)
}

#[tauri::command]
fn set_voice_flags(state: State<'_, AppState>, self_mute: bool, self_deaf: bool) -> Result<(), String> {
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    settings.self_mute = self_mute;
    settings.self_deaf = self_deaf;
    settings.to_config().map_err(|err| err.to_string())?;
    settings.save(&state.settings_path).map_err(|err| err.to_string())?;
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.apply(settings.runtime())?;
    }
    Ok(())
}

#[tauri::command]
fn snapshot(state: State<'_, AppState>) -> Snapshot {
    state
        .bridge
        .lock()
        .expect("bridge lock")
        .as_ref()
        .map(BridgeHandle::snapshot)
        .unwrap_or_else(Snapshot::disconnected)
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState {
                bridge: Mutex::new(None),
                settings_path: default_settings_path(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            connect,
            disconnect,
            join_voice,
            leave_voice,
            assign_slot,
            clear_slot,
            set_voice_flags,
            snapshot
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Discord OMT Bridge")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                if let Some(state) = app.try_state::<AppState>() {
                    stop_bridge(&state);
                }
            }
        });
}
