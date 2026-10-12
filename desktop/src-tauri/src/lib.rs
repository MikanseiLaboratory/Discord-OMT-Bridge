use std::path::PathBuf;
use std::sync::Mutex;

mod logs;

use discord_omt_bridge::{
    default_settings_path, list_output_devices, login_password, send_login_sms, spawn_session, submit_mfa, AuthStep,
    AudioSource, BridgeHandle, CaptchaAnswer, OutputRoute, PreviewFrame, Settings, Snapshot, SourceKind, Transport,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, RunEvent, State, Url, WebviewUrl, WebviewWindowBuilder};

struct AppState {
    bridge: Mutex<Option<BridgeHandle>>,
    settings_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsDto {
    token: String,
    self_deaf: bool,
    allow_user_ids: Vec<String>,
    deny_user_ids: Vec<String>,
    max_video_slots: u32,
    idle_timeout_secs: u64,
    client_build_number: u32,
    #[serde(default)]
    outputs: Vec<OutputDto>,
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutputDto {
    id: String,
    name: String,
    kind: String,
    channel_id: String,
    user_id: String,
    #[serde(default = "default_saved_transport")]
    transport: String,
    #[serde(default)]
    device: String,
    #[serde(default = "default_channel_start")]
    channel_start: u16,
    #[serde(default = "default_channel_count")]
    channel_count: u16,
    #[serde(default)]
    members: Vec<AudioMemberDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioMemberDto {
    channel_id: String,
    user_id: String,
    #[serde(default = "default_volume")]
    volume: u16,
}

fn default_volume() -> u16 {
    100
}

fn default_saved_transport() -> String {
    "omt".into()
}

fn default_channel_start() -> u16 {
    1
}

fn default_channel_count() -> u16 {
    2
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
        self_deaf: settings.self_deaf,
        allow_user_ids: settings.allow_user_ids.iter().map(|id| id.to_string()).collect(),
        deny_user_ids: settings.deny_user_ids.iter().map(|id| id.to_string()).collect(),
        max_video_slots: settings.max_video_slots as u32,
        idle_timeout_secs: settings.idle_timeout_secs,
        client_build_number: settings.client_build_number,
        outputs: settings
            .outputs
            .iter()
            .map(|output| OutputDto {
                id: output.id.clone(),
                name: output.name.clone(),
                kind: output.kind.clone(),
                channel_id: output.channel_id.to_string(),
                user_id: output.user_id.to_string(),
                transport: output.transport.clone(),
                device: output.device.clone(),
                channel_start: output.channel_start,
                channel_count: output.channel_count,
                members: output
                    .members
                    .iter()
                    .map(|member| AudioMemberDto {
                        channel_id: member.channel_id.to_string(),
                        user_id: member.user_id.to_string(),
                        volume: member.volume,
                    })
                    .collect(),
            })
            .collect(),
        path: path.display().to_string(),
    }
}

fn from_dto(dto: &SettingsDto) -> Result<Settings, String> {
    Ok(Settings {
        token: dto.token.trim().to_owned(),
        self_deaf: dto.self_deaf,
        allow_user_ids: parse_ids(&dto.allow_user_ids)?,
        deny_user_ids: parse_ids(&dto.deny_user_ids)?,
        max_video_slots: dto.max_video_slots.clamp(1, 32) as usize,
        idle_timeout_secs: dto.idle_timeout_secs.max(1),
        client_build_number: dto.client_build_number,
        outputs: outputs_from_dto(&dto.outputs)?,
    })
}

fn parse_route_id(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() || value == "0" {
        return Ok(0);
    }
    value.parse::<u64>().map_err(|_| format!("ID が不正です: {value}"))
}

fn outputs_from_dto(values: &[OutputDto]) -> Result<Vec<OutputRoute>, String> {
    if values.len() > 64 {
        return Err("出力は64個までです".into());
    }
    let mut outputs = Vec::with_capacity(values.len());
    for output in values {
        let id = output.id.trim();
        let name = output.name.trim();
        if id.is_empty() || name.is_empty() {
            return Err("名前を入力してください".into());
        }
        let kind = SourceKind::parse(&output.kind)?.slug().to_owned();
        let transport = Transport::parse(&output.transport)?.slug().to_owned();
        if outputs.iter().any(|existing: &OutputRoute| existing.id == id || existing.name == name) {
            return Err("同じ名前の出力があります".into());
        }
        outputs.push(OutputRoute {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
            channel_id: parse_route_id(&output.channel_id)?,
            user_id: parse_route_id(&output.user_id)?,
            transport,
            device: output.device.trim().to_owned(),
            channel_start: output.channel_start.max(1),
            channel_count: output.channel_count.clamp(1, 64),
            members: members_from_dto(&output.members)?,
        });
    }
    Ok(outputs)
}

fn members_from_dto(values: &[AudioMemberDto]) -> Result<Vec<AudioSource>, String> {
    let mut members = Vec::with_capacity(values.len());
    for member in values {
        let channel_id = parse_route_id(&member.channel_id)?;
        let user_id = parse_route_id(&member.user_id)?;
        if channel_id == 0 || user_id == 0 {
            continue;
        }
        if members.iter().any(|existing: &AudioSource| existing.channel_id == channel_id && existing.user_id == user_id) {
            continue;
        }
        members.push(AudioSource {
            channel_id,
            user_id,
            volume: member.volume.min(200),
        });
    }
    Ok(members)
}

fn video_kind(kind: &str) -> bool {
    matches!(kind, "camera" | "golive")
}

fn new_output_id(existing: &[OutputRoute]) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut id = format!("out-{nanos}");
    let mut suffix = 0u32;
    while existing.iter().any(|output| output.id == id) {
        suffix += 1;
        id = format!("out-{nanos}-{suffix}");
    }
    id
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
    handle.assign_slot(slot_id, channel_id, user_id, String::new())
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
fn add_output(state: State<'_, AppState>, name: String, kind: String, transport: String) -> Result<String, String> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err("名前を入力してください".into());
    }
    let kind = SourceKind::parse(&kind)?.slug().to_owned();
    let transport = Transport::parse(&transport)?.slug().to_owned();
    if video_kind(&kind) && matches!(transport.as_str(), "device" | "asio") {
        return Err("映像は OMT か NDI に出します".into());
    }
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    if settings.outputs.len() >= 64 {
        return Err("出力は64個までです".into());
    }
    if settings.outputs.iter().any(|output| output.name == name) {
        return Err("同じ名前の出力があります".into());
    }
    let id = new_output_id(&settings.outputs);
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        if let Err(err) = handle.add_output(id.clone(), name.clone(), kind.clone(), transport.clone(), String::new(), 1, 2) {
            return Err(err);
        }
    }
    settings.outputs.push(OutputRoute {
        id: id.clone(),
        name,
        kind,
        channel_id: 0,
        user_id: 0,
        transport,
        device: String::new(),
        channel_start: 1,
        channel_count: 2,
        members: Vec::new(),
    });
    if let Err(err) = settings.save(&state.settings_path) {
        if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
            let _ = handle.remove_output(id);
        }
        return Err(err.to_string());
    }
    Ok(id)
}

#[tauri::command]
fn remove_output(state: State<'_, AppState>, output_id: String) -> Result<(), String> {
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    if !settings.outputs.iter().any(|output| output.id == output_id) {
        return Err("出力が見つかりません".into());
    }
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.remove_output(output_id.clone())?;
    }
    settings.outputs.retain(|output| output.id != output_id);
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[tauri::command]
fn rename_output(state: State<'_, AppState>, output_id: String, name: String) -> Result<(), String> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err("名前を入力してください".into());
    }
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    if settings.outputs.iter().any(|output| output.name == name && output.id != output_id) {
        return Err("同じ名前の出力があります".into());
    }
    let Some(index) = settings.outputs.iter().position(|output| output.id == output_id) else {
        return Err("出力が見つかりません".into());
    };
    if settings.outputs[index].name == name {
        return Ok(());
    }
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.rename_output(output_id, name.clone())?;
    }
    settings.outputs[index].name = name;
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[tauri::command]
fn set_output_route(
    state: State<'_, AppState>,
    output_id: String,
    channel_id: String,
    user_id: String,
    kind: String,
) -> Result<(), String> {
    let channel_id = parse_route_id(&channel_id)?;
    let user_id = parse_route_id(&user_id)?;
    let kind = if kind.trim().is_empty() {
        String::new()
    } else {
        SourceKind::parse(&kind)?.slug().to_owned()
    };
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    let Some(index) = settings.outputs.iter().position(|output| output.id == output_id) else {
        return Err("出力が見つかりません".into());
    };
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        if channel_id == 0 || user_id == 0 {
            handle.clear_slot(output_id.clone())?;
        } else {
            handle.assign_slot(output_id, channel_id, user_id, kind.clone())?;
        }
    }
    if !kind.is_empty() {
        settings.outputs[index].kind = kind;
    }
    if channel_id == 0 || user_id == 0 {
        settings.outputs[index].channel_id = 0;
        settings.outputs[index].user_id = 0;
    } else {
        settings.outputs[index].channel_id = channel_id;
        settings.outputs[index].user_id = user_id;
    }
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[tauri::command]
fn set_output_transport(state: State<'_, AppState>, output_id: String, transport: String) -> Result<(), String> {
    let transport = Transport::parse(&transport)?.slug().to_owned();
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    let Some(index) = settings.outputs.iter().position(|output| output.id == output_id) else {
        return Err("出力が見つかりません".into());
    };
    if settings.outputs[index].transport == transport {
        return Ok(());
    }
    if video_kind(&settings.outputs[index].kind) && matches!(transport.as_str(), "device" | "asio") {
        return Err("映像は OMT か NDI に出します".into());
    }
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.set_transport(output_id, transport.clone())?;
    }
    settings.outputs[index].transport = transport;
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[tauri::command]
fn set_output_playback(
    state: State<'_, AppState>,
    output_id: String,
    device: String,
    channel_start: u16,
    channel_count: u16,
) -> Result<(), String> {
    let device = device.trim().to_owned();
    let channel_start = channel_start.max(1);
    let channel_count = channel_count.clamp(1, 64);
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    let Some(index) = settings.outputs.iter().position(|output| output.id == output_id) else {
        return Err("出力が見つかりません".into());
    };
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.set_playback(output_id, device.clone(), channel_start, channel_count)?;
    }
    settings.outputs[index].device = device;
    settings.outputs[index].channel_start = channel_start;
    settings.outputs[index].channel_count = channel_count;
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[tauri::command]
fn set_audio_member(
    state: State<'_, AppState>,
    output_id: String,
    channel_id: String,
    user_id: String,
    volume: u16,
    included: bool,
) -> Result<(), String> {
    let channel_id = parse_route_id(&channel_id)?;
    let user_id = parse_route_id(&user_id)?;
    let volume = volume.min(200);
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    let Some(index) = settings.outputs.iter().position(|output| output.id == output_id) else {
        return Err("出力が見つかりません".into());
    };
    if settings.outputs[index].kind != "audio" {
        return Err("人を混ぜられるのは音声だけです".into());
    }
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.set_audio_member(output_id, channel_id, user_id, volume, included)?;
    }
    let members = &mut settings.outputs[index].members;
    members.retain(|member| !(member.channel_id == channel_id && member.user_id == user_id));
    if included && channel_id != 0 && user_id != 0 {
        members.push(AudioSource { channel_id, user_id, volume });
    }
    settings.save(&state.settings_path).map_err(|err| err.to_string())
}

#[derive(Serialize)]
struct PlaybackDeviceDto {
    name: String,
    host: String,
    channels: u16,
}

#[tauri::command]
fn playback_devices() -> Vec<PlaybackDeviceDto> {
    list_output_devices()
        .into_iter()
        .map(|device| PlaybackDeviceDto {
            name: device.name,
            host: device.host.to_owned(),
            channels: device.channels,
        })
        .collect()
}

#[tauri::command]
fn recent_logs() -> Vec<logs::LogLine> {
    logs::recent()
}

#[tauri::command]
fn set_voice_flags(state: State<'_, AppState>, self_deaf: bool) -> Result<(), String> {
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    settings.self_deaf = self_deaf;
    settings.to_config().map_err(|err| err.to_string())?;
    settings.save(&state.settings_path).map_err(|err| err.to_string())?;
    if let Some(handle) = state.bridge.lock().expect("bridge lock").as_ref() {
        handle.apply(settings.runtime())?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct AuthDto {
    kind: String,
    token: String,
    ticket: String,
    totp: bool,
    sms: bool,
    backup: bool,
    phone: String,
    service: String,
    sitekey: String,
    rqdata: String,
    rqtoken: String,
    session_id: String,
}

fn captcha_answer(key: Option<String>, rqtoken: Option<String>, session_id: Option<String>) -> Option<CaptchaAnswer> {
    let key = key.unwrap_or_default();
    if key.trim().is_empty() {
        return None;
    }
    let blank = |value: Option<String>| {
        let value = value.unwrap_or_default();
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    };
    Some(CaptchaAnswer {
        key,
        rqtoken: blank(rqtoken),
        session_id: blank(session_id),
    })
}

fn auth_dto(step: AuthStep) -> AuthDto {
    let mut dto = AuthDto {
        kind: String::new(),
        token: String::new(),
        ticket: String::new(),
        totp: false,
        sms: false,
        backup: false,
        phone: String::new(),
        service: String::new(),
        sitekey: String::new(),
        rqdata: String::new(),
        rqtoken: String::new(),
        session_id: String::new(),
    };
    match step {
        AuthStep::Token(token) => {
            dto.kind = "token".into();
            dto.token = token;
        }
        AuthStep::Mfa { ticket, totp, sms, backup } => {
            dto.kind = "mfa".into();
            dto.ticket = ticket;
            dto.totp = totp;
            dto.sms = sms;
            dto.backup = backup;
        }
        AuthStep::Captcha(challenge) => {
            dto.kind = "captcha".into();
            dto.service = challenge.service;
            dto.sitekey = challenge.sitekey;
            dto.rqdata = challenge.rqdata;
            dto.rqtoken = challenge.rqtoken;
            dto.session_id = challenge.session_id;
        }
        AuthStep::SmsSent { phone } => {
            dto.kind = "sms".into();
            dto.phone = phone;
        }
    }
    dto
}

fn client_build(state: &State<'_, AppState>) -> u32 {
    Settings::load(&state.settings_path)
        .map(|settings| settings.client_build_number)
        .unwrap_or(0)
}

#[tauri::command]
async fn discord_login(
    state: State<'_, AppState>,
    email: String,
    password: String,
    captcha_key: Option<String>,
    captcha_rqtoken: Option<String>,
    captcha_session_id: Option<String>,
) -> Result<AuthDto, String> {
    let build = client_build(&state);
    let step = login_password(
        &email,
        &password,
        build,
        captcha_answer(captcha_key, captcha_rqtoken, captcha_session_id),
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(auth_dto(step))
}

#[tauri::command]
async fn discord_mfa(
    state: State<'_, AppState>,
    ticket: String,
    code: String,
    method: String,
    captcha_key: Option<String>,
    captcha_rqtoken: Option<String>,
    captcha_session_id: Option<String>,
) -> Result<AuthDto, String> {
    let build = client_build(&state);
    let step = submit_mfa(
        &ticket,
        &code,
        &method,
        build,
        captcha_answer(captcha_key, captcha_rqtoken, captcha_session_id),
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(auth_dto(step))
}

#[tauri::command]
async fn discord_sms(
    state: State<'_, AppState>,
    ticket: String,
    captcha_key: Option<String>,
    captcha_rqtoken: Option<String>,
    captcha_session_id: Option<String>,
) -> Result<AuthDto, String> {
    let build = client_build(&state);
    let step = send_login_sms(
        &ticket,
        build,
        captcha_answer(captcha_key, captcha_rqtoken, captcha_session_id),
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(auth_dto(step))
}

const LOGIN_HOOK: &str = r#"
(() => {
  if (window.__omtLoginHook) return;
  window.__omtLoginHook = true;
  const report = (token) => {
    if (window.__omtReported) return;
    token = String(token || "").trim().replace(/^"|"$/g, "");
    if (token.length < 50 || /\s/.test(token)) return;
    const invoke = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
    if (!invoke) return;
    window.__omtReported = true;
    Promise.resolve(invoke("complete_discord_login", { token })).catch(() => {
      window.__omtReported = false;
    });
  };
  const inspect = (text) => {
    try {
      const data = JSON.parse(text);
      if (data && typeof data.token === "string") report(data.token);
    } catch (e) {}
  };
  const originalFetch = window.fetch;
  if (originalFetch) {
    window.fetch = function () {
      return originalFetch.apply(this, arguments).then((response) => {
        try {
          const target = arguments[0] && arguments[0].url ? arguments[0].url : arguments[0];
          if (String(target || "").indexOf("/auth/") !== -1) {
            response.clone().text().then(inspect).catch(() => {});
          }
        } catch (e) {}
        return response;
      });
    };
  }
  const originalOpen = XMLHttpRequest.prototype.open;
  const originalSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (method, url) {
    this.__omtUrl = String(url || "");
    return originalOpen.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    this.addEventListener("load", function () {
      if (String(this.__omtUrl || "").indexOf("/auth/") !== -1) inspect(this.responseText);
    });
    return originalSend.apply(this, arguments);
  };
  setInterval(() => {
    try {
      const raw = localStorage.getItem("token");
      if (!raw || raw === "null") return;
      let token = raw;
      try {
        const parsed = JSON.parse(raw);
        if (typeof parsed === "string") token = parsed;
      } catch (e) {}
      report(token);
    } catch (e) {}
  }, 500);
})();
"#;

const LOGIN_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

#[tauri::command]
async fn open_discord_login(app: tauri::AppHandle) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let app_for_window = app.clone();
    app.run_on_main_thread(move || {
        let app = app_for_window;
        let result = (|| -> Result<(), String> {
            if let Some(window) = app.get_webview_window("discord-login") {
                let _ = window.unminimize();
                window.show().map_err(|err| err.to_string())?;
                window.set_focus().map_err(|err| err.to_string())?;
                return Ok(());
            }
            let url = Url::parse("https://discord.com/login").map_err(|err| err.to_string())?;
            WebviewWindowBuilder::new(&app, "discord-login", WebviewUrl::External(url))
                .title("Discord にログイン")
                .inner_size(520.0, 820.0)
                .min_inner_size(420.0, 640.0)
                .center()
                .resizable(true)
                .closable(true)
                .decorations(true)
                .focused(true)
                .user_agent(LOGIN_USER_AGENT)
                .initialization_script(LOGIN_HOOK)
                .build()
                .map_err(|err| err.to_string())?;
            Ok(())
        })();
        let _ = tx.send(result);
    })
    .map_err(|err| err.to_string())?;
    rx.recv().map_err(|_| "ログイン画面を開けませんでした".to_owned())?
}

#[tauri::command]
fn complete_discord_login(app: tauri::AppHandle, state: State<'_, AppState>, token: String) -> Result<(), String> {
    let token = token.trim().trim_matches('"').trim().to_owned();
    if token.len() < 50 || token.chars().any(char::is_whitespace) {
        return Err("ログイン結果を読み取れませんでした".into());
    }
    let mut settings = Settings::load(&state.settings_path).map_err(|err| err.to_string())?;
    settings.token = token;
    settings.to_config().map_err(|err| err.to_string())?;
    settings.save(&state.settings_path).map_err(|err| err.to_string())?;
    app.emit("discord-login-finished", ())
        .map_err(|err| err.to_string())?;
    let closer = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = closer.get_webview_window("discord-login") {
            let _ = window.close();
        }
    });
    Ok(())
}

#[tauri::command]
fn previews(state: State<'_, AppState>) -> Vec<PreviewFrame> {
    state
        .bridge
        .lock()
        .expect("bridge lock")
        .as_ref()
        .map(BridgeHandle::previews)
        .unwrap_or_default()
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

pub fn init_tracing() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(logs::LogCapture)
        .try_init();
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
            add_output,
            remove_output,
            rename_output,
            set_output_route,
            set_output_transport,
            set_output_playback,
            set_audio_member,
            playback_devices,
            recent_logs,
            set_voice_flags,
            discord_login,
            discord_mfa,
            discord_sms,
            open_discord_login,
            complete_discord_login,
            previews,
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
