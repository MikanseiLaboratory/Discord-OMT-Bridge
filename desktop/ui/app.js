const invoke = (...args) => window.__TAURI__.core.invoke(...args);

const state = {
  mode: "gate",
  settingsTab: "account",
  selectedGuildId: null,
  selectedChannelId: null,
  picked: null,
  dirty: false,
  snapshot: emptySnapshot(),
  settings: null,
  lastRenderKey: "",
};

function emptySnapshot() {
  return {
    phase: "disconnected",
    connected: false,
    self_user: null,
    guilds: [],
    seats: [],
    slots: [],
    last_error: null,
    ndi_available: false,
    self_mute: true,
    self_deaf: false,
  };
}

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[ch]));
}

function colorFromId(id) {
  const colors = ["#5865f2", "#57f287", "#fee75c", "#eb459e", "#ed4245", "#3ba55c", "#faa61a"];
  let hash = 0;
  for (const ch of String(id)) hash = (hash * 33 + ch.charCodeAt(0)) >>> 0;
  return colors[hash % colors.length];
}

function avatarUrl(id, hash, size) {
  if (!hash) return "";
  const ext = hash.startsWith("a_") ? "gif" : "png";
  return `https://cdn.discordapp.com/avatars/${id}/${hash}.${ext}?size=${size}`;
}

function guildIconUrl(id, icon) {
  if (!icon) return "";
  const ext = icon.startsWith("a_") ? "gif" : "png";
  return `https://cdn.discordapp.com/icons/${id}/${icon}.${ext}?size=96`;
}

function initial(name) {
  const text = (name || "?").trim();
  return esc(Array.from(text)[0] || "?");
}

function avatarHtml(id, hash, name, large) {
  const cls = large ? "avatar-lg" : "avatar";
  const url = avatarUrl(id, hash, large ? 128 : 64);
  const inner = url ? `<img src="${esc(url)}" alt="" />` : initial(name);
  return `<span class="${cls}" style="background:${colorFromId(id)}">${inner}</span>`;
}

function phaseLabel(phase) {
  switch (phase) {
    case "connecting": return "接続しています";
    case "online": return "オンライン";
    case "voice": return "ボイス接続";
    case "reconnecting": return "再接続しています";
    default: return "オフライン";
  }
}

function kindLabel(kind) {
  if (kind === "Audio") return "マイク";
  if (kind === "Camera") return "カメラ";
  if (kind === "GoLive") return "Go Live";
  return kind;
}

function selectedGuild() {
  return state.snapshot.guilds.find((guild) => guild.id === state.selectedGuildId) || null;
}

function findChannel(channelId) {
  for (const guild of state.snapshot.guilds) {
    const channel = guild.channels.find((item) => item.id === channelId);
    if (channel) return { guild, channel };
  }
  return null;
}

function channelTree(channels) {
  const visible = channels.filter((channel) => channel.kind !== "other");
  const byPos = (a, b) => a.position - b.position || a.name.localeCompare(b.name, "ja");
  const categories = visible.filter((channel) => channel.kind === "category").sort(byPos);
  const rest = visible.filter((channel) => channel.kind !== "category");
  const sections = [];
  const loose = rest.filter((channel) => !channel.parent_id).sort(byPos);
  if (loose.length) sections.push({ category: null, channels: loose });
  const placed = new Set(loose.map((channel) => channel.id));
  for (const category of categories) {
    const children = rest.filter((channel) => channel.parent_id === category.id).sort(byPos);
    children.forEach((channel) => placed.add(channel.id));
    sections.push({ category, channels: children });
  }
  const orphans = rest.filter((channel) => !placed.has(channel.id)).sort(byPos);
  if (orphans.length) sections.push({ category: null, channels: orphans });
  return sections;
}

function iconSpeaker() {
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M11 5 6 9H3v6h3l5 4V5zm2.5 3.5a4.5 4.5 0 0 1 0 7" fill="none" stroke="currentColor" stroke-width="2"/></svg>`;
}

function iconHash() {
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M5 9h14M5 15h14M9 4 7 20M17 4l-2 16"/></svg>`;
}

function iconHome() {
  return `<svg width="26" height="26" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M4 10v9h6v-5h4v5h6v-9l-8-6-8 6z"/></svg>`;
}

function iconMic(muted) {
  const slash = muted ? `<path d="M4 4l16 16" fill="none" stroke="currentColor" stroke-width="2"/>` : "";
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 14a3 3 0 0 0 3-3V6a3 3 0 0 0-6 0v5a3 3 0 0 0 3 3zm5-3a5 5 0 0 1-10 0H5a7 7 0 0 0 6 6.9V21h2v-3.1A7 7 0 0 0 19 11h-2z"/>${slash}</svg>`;
}

function iconHeadphones(deaf) {
  const slash = deaf ? `<path d="M4 4l16 16" fill="none" stroke="currentColor" stroke-width="2"/>` : "";
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 3a8 8 0 0 0-8 8v6a2 2 0 0 0 2 2h2v-7H6v-1a6 6 0 0 1 12 0v1h-2v7h2a2 2 0 0 0 2-2v-6a8 8 0 0 0-8-8z"/>${slash}</svg>`;
}

function iconLeave() {
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M7 4h6a2 2 0 0 1 2 2v2h-2V6H7v12h6v-2h2v2a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zm8.3 4.3 1.4 1.4L15.4 11H22v2h-6.6l1.3 1.3-1.4 1.4L11.6 12l3.7-3.7z"/></svg>`;
}

function iconGear() {
  return `<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M19.1 12.9a7.7 7.7 0 0 0 .1-1.8l2-1.5-2-3.4-2.4 1a7.8 7.8 0 0 0-1.6-.9l-.4-2.5h-4l-.4 2.5c-.5.2-1.1.5-1.6.9l-2.4-1-2 3.4 2 1.5a7.7 7.7 0 0 0 0 1.8l-2 1.5 2 3.4 2.4-1c.5.4 1 .7 1.6.9l.4 2.5h4l.4-2.5c.5-.2 1.1-.5 1.6-.9l2.4 1 2-3.4-2-1.5zM12 15.5A3.5 3.5 0 1 1 12 8a3.5 3.5 0 0 1 0 7.5z"/></svg>`;
}

function bannerHtml() {
  if (!state.snapshot.last_error) return "";
  return `<div class="banner">${esc(state.snapshot.last_error)}</div>`;
}

function serverColumn() {
  const guilds = state.snapshot.guilds.map((guild) => {
    const active = guild.id === state.selectedGuildId ? " active" : "";
    const icon = guildIconUrl(guild.id, guild.icon);
    const face = icon
      ? `<img src="${esc(icon)}" alt="" />`
      : initial(guild.name);
    return `<div class="server-wrap${active}"><button class="server-button" data-action="guild" data-id="${esc(guild.id)}" title="${esc(guild.name)}" style="background:${icon ? "#313338" : colorFromId(guild.id)}">${face}</button></div>`;
  }).join("");
  const homeActive = state.selectedGuildId ? "" : " active";
  return `<aside class="servers">
    <div class="server-wrap${homeActive}"><button class="server-button home" data-action="home" title="ホーム">${iconHome()}</button></div>
    <div class="server-separator"></div>
    <div class="server-scroll">${guilds}</div>
  </aside>`;
}

function seatFor(channelId) {
  return (state.snapshot.seats || []).find((seat) => seat.channel_id === channelId) || null;
}

function channelButton(guild, channel) {
  const joined = Boolean(seatFor(channel.id));
  const active = state.selectedChannelId === channel.id;
  const classes = ["channel", active ? "active" : "", joined ? "joined" : ""].filter(Boolean).join(" ");
  const icon = channel.kind === "voice" || channel.kind === "stage" ? iconSpeaker() : iconHash();
  const members = (channel.members || []).map((member) => {
    const live = member.live ? `<span class="live-pill">LIVE</span>` : "";
    const picked = state.picked && state.picked.userId === member.id && state.picked.channelId === channel.id ? " picked" : "";
    return `<div class="voice-user${picked}" data-action="pick" data-channel="${esc(channel.id)}" data-user="${esc(member.id)}" data-name="${esc(member.name)}">${avatarHtml(member.id, member.avatar, member.name, false)}<span>${esc(member.name)}</span>${live}</div>`;
  }).join("");
  return `<button class="${classes}" data-action="channel" data-guild="${esc(guild.id)}" data-id="${esc(channel.id)}" data-kind="${esc(channel.kind)}">${icon}<span>${esc(channel.name)}</span></button>${members}`;
}

function channelColumn() {
  const guild = selectedGuild();
  const title = guild ? esc(guild.name) : "ホーム";
  let body = `<div class="empty">サーバーを選ぶとチャンネルが表示されます。</div>`;
  if (guild) {
    if (!guild.channels.length && state.snapshot.phase === "connecting") {
      body = `<div class="empty">チャンネルを受信しています。</div>`;
    } else {
      body = channelTree(guild.channels).map((section) => {
        const heading = section.category ? `<div class="category">${esc(section.category.name)}</div>` : "";
        return heading + section.channels.map((channel) => channelButton(guild, channel)).join("");
      }).join("");
    }
  }
  return `<aside class="channels">
    <div class="guild-name">${title}</div>
    ${bannerHtml()}
    <div class="channel-scroll">${body}</div>
    ${voiceBar()}
    ${userPanel()}
  </aside>`;
}

function voiceBar() {
  const seats = state.snapshot.seats || [];
  if (!seats.length) return "";
  const snap = state.snapshot;
  const rows = seats.map((seat) => {
    const found = findChannel(seat.channel_id);
    const name = found ? found.channel.name : seat.channel_id;
    const mark = seat.connected ? "" : " · 切断";
    return `<div class="seat">
      <div class="where">${esc(name)}${mark}</div>
      <button class="icon-button leave" data-action="leave" data-guild="${esc(seat.guild_id)}" data-channel="${esc(seat.channel_id)}" title="このチャンネルから切断">${iconLeave()}</button>
    </div>`;
  }).join("");
  return `<div class="voice-bar">
    <div>${esc(phaseLabel(snap.phase))}</div>
    ${rows}
    <div class="row">
      <button class="icon-button${snap.self_mute ? " toggled" : ""}" data-action="toggle-mute" title="ミュート">${iconMic(snap.self_mute)}</button>
      <button class="icon-button${snap.self_deaf ? " toggled" : ""}" data-action="toggle-deaf" title="スピーカーミュート">${iconHeadphones(snap.self_deaf)}</button>
    </div>
  </div>`;
}

function userPanel() {
  const user = state.snapshot.self_user;
  const name = user ? user.name : "未接続";
  const id = user ? user.id : "0";
  const hash = user ? user.avatar : null;
  const online = state.snapshot.phase === "online" || state.snapshot.phase === "voice";
  return `<div class="user-panel">
    <div class="avatar-wrap">${avatarHtml(id, hash, name, false)}<span class="presence${online ? " on" : ""}"></span></div>
    <div class="who"><div class="name">${esc(name)}</div><div class="status">${esc(phaseLabel(state.snapshot.phase))}</div></div>
    <button class="icon-button${state.snapshot.self_mute ? " toggled" : ""}" data-action="toggle-mute" title="ミュート">${iconMic(state.snapshot.self_mute)}</button>
    <button class="icon-button${state.snapshot.self_deaf ? " toggled" : ""}" data-action="toggle-deaf" title="スピーカーミュート">${iconHeadphones(state.snapshot.self_deaf)}</button>
    <button class="icon-button" data-action="open-settings" title="ユーザー設定">${iconGear()}</button>
  </div>`;
}

function sourceCards() {
  const slots = state.snapshot.slots || [];
  if (!slots.length) {
    return `<div class="empty">接続すると、マイク・カメラ・Go Live の送出先が作られます。カメラを切っても送出先は残ります。</div>`;
  }
  const picked = state.picked
    ? `<div class="hint">選択中: ${esc(state.picked.name)}。スロットを押すと、その人を固定します。</div>`
    : `<div class="hint">メンバーを選んでからスロットを押すと、その人の音声や映像を固定します。</div>`;
  const cards = slots.map((slot) => {
    const who = slot.assigned_name || (slot.user_id ? `user-${slot.user_id}` : "空き");
    const flags = [
      slot.pinned ? "固定" : "",
      slot.live ? "受信中" : "待機",
    ].filter(Boolean).join(" · ");
    return `<div class="source-card">
      <div class="kind">${esc(kindLabel(slot.kind))}</div>
      <div class="sname">${esc(slot.name)}</div>
      <div class="meta">${esc(who)} / ${esc(flags)}</div>
      <div class="meta">${slot.frames} フレーム / 欠落 ${slot.dropped} / ${Number(slot.fps).toFixed(1)} fps</div>
      <div class="slot-actions">
        <button class="secondary" type="button" data-action="assign-slot" data-id="${esc(slot.id)}" ${state.picked ? "" : "disabled"}>ここに当てる</button>
        <button class="secondary" type="button" data-action="clear-slot" data-id="${esc(slot.id)}" ${slot.user_id ? "" : "disabled"}>外す</button>
      </div>
    </div>`;
  }).join("");
  return picked + cards;
}

function mainColumn() {
  const guild = selectedGuild();
  const found = guild && state.selectedChannelId
    ? guild.channels.find((channel) => channel.id === state.selectedChannelId)
    : null;
  if (!guild) {
    return `<main class="main">
      <div class="pane-header">ホーム</div>
      <div class="home-body">
        <h2>Discord OMT Bridge</h2>
        <p>左のサーバーからボイスチャンネルを選ぶと、その通話が席に追加されます。複数のチャンネルへ同時に参加できます。マイク、カメラ、Go Live は、あらかじめ作った OMT / NDI の送出先へ当てます。カメラや Go Live を付け直しても送出先は消えません。テキストは送受信しません。</p>
        <p>状態: ${esc(phaseLabel(state.snapshot.phase))}</p>
        ${state.snapshot.phase === "disconnected" ? `<div class="actions"><button class="primary" data-action="connect">接続</button><button class="secondary" data-action="open-settings">トークンと設定</button></div>` : `<div class="actions"><button class="secondary" data-action="disconnect">切断</button></div>`}
        <h2>送出先</h2>
        ${sourceCards()}
      </div>
    </main>`;
  }
  if (!found) {
    return `<main class="main"><div class="pane-header">${esc(guild.name)}</div><div class="empty">チャンネルを選択してください。未参加のボイスチャンネルを選ぶと、その通話が席に追加されます。</div></main>`;
  }
  const icon = found.kind === "voice" || found.kind === "stage" ? iconSpeaker() : iconHash();
  if (found.kind === "text") {
    return `<main class="main"><div class="pane-header">${icon} ${esc(found.name)}</div><div class="empty">このアプリはテキストチャンネルの読み書きをしません。映像と音声はボイスチャンネルから受けます。</div></main>`;
  }
  const tiles = (found.members || []).map((member) => {
    const badges = [
      member.camera ? "カメラ" : "",
      member.live ? "LIVE" : "",
      member.mute ? "ミュート" : "",
    ].filter(Boolean).join(" · ");
    const picked = state.picked && state.picked.userId === member.id && state.picked.channelId === found.id ? " picked" : "";
    return `<button class="tile${picked}" type="button" data-action="pick" data-channel="${esc(found.id)}" data-user="${esc(member.id)}" data-name="${esc(member.name)}">${avatarHtml(member.id, member.avatar, member.name, true)}<div class="caption"><span>${esc(member.name)}</span>${badges ? `<span>${esc(badges)}</span>` : ""}</div></button>`;
  }).join("");
  const body = tiles || `<div class="empty">このチャンネルには、まだ誰も表示されていません。</div>`;
  return `<main class="main"><div class="pane-header">${icon} ${esc(found.name)}</div><div class="stage"><div class="tiles">${body}</div></div></main>`;
}

function sourceColumn() {
  if (!state.selectedGuildId) return "";
  return `<aside class="sources"><div class="pane-header">送出先</div><div class="source-scroll">${sourceCards()}</div></aside>`;
}

function shellHtml() {
  const layout = state.selectedGuildId ? "shell" : "shell home-layout";
  return `<div class="${layout}">${serverColumn()}${channelColumn()}${mainColumn()}${sourceColumn()}</div>`;
}

function renderShell() {
  const key = JSON.stringify({
    snapshot: state.snapshot,
    guild: state.selectedGuildId,
    channel: state.selectedChannelId,
    picked: state.picked,
  });
  if (key === state.lastRenderKey) return;
  state.lastRenderKey = key;
  const serverScroll = document.querySelector(".server-scroll")?.scrollTop ?? 0;
  const channelScroll = document.querySelector(".channel-scroll")?.scrollTop ?? 0;
  document.getElementById("app").innerHTML = shellHtml();
  const server = document.querySelector(".server-scroll");
  const channel = document.querySelector(".channel-scroll");
  if (server) server.scrollTop = serverScroll;
  if (channel) channel.scrollTop = channelScroll;
}

function settingsForm() {
  const settings = state.settings || {};
  const ndiDisabled = state.snapshot.ndi_available ? "" : "disabled";
  const tab = state.settingsTab;
  const account = `<section class="settings-card" ${tab === "account" ? "" : "hidden"}>
    <h2>アカウント</h2>
    <p class="hint">ユーザートークンで自分のアカウントとして参加します。Discord の利用規約に反し、アカウント停止の対象です。専用アカウントだけを使ってください。</p>
    <div class="warning">トークンは ${esc(settings.path || "")} に保存します。ログには出しません。</div>
    <label class="field">ユーザートークン<input id="token" type="password" value="${esc(settings.token)}" autocomplete="off" /></label>
    <label class="field">クライアント build number<input id="build" type="number" min="0" value="${esc(settings.client_build_number || 0)}" /><span class="hint">0 のときは起動時に安定版の番号を取ります。変更は再接続後に使われます。</span></label>
    <div class="actions">
      <button class="primary" type="button" data-action="connect">接続</button>
      <button class="secondary" type="button" data-action="disconnect">切断</button>
    </div>
  </section>`;
  const voice = `<section class="settings-card" ${tab === "voice" ? "" : "hidden"}>
    <h2>ボイスと送出</h2>
    <label class="check"><input id="self-mute" type="checkbox" ${settings.self_mute ? "checked" : ""} />参加時にマイクをミュートする</label>
    <label class="check"><input id="self-deaf" type="checkbox" ${settings.self_deaf ? "checked" : ""} />スピーカーをミュートする</label>
    <label class="check"><input id="enable-omt" type="checkbox" ${settings.enable_omt ? "checked" : ""} />OMT へ送る</label>
    <label class="check"><input id="enable-ndi" type="checkbox" ${settings.enable_ndi ? "checked" : ""} ${ndiDisabled} />NDI へ送る</label>
    <p class="hint">${state.snapshot.ndi_available ? "NDI と OMT は同じソースを並行して出します。" : "このビルドには NDI が含まれていません。NDI 6 SDK がある環境で --features ndi を付けてビルドしてください。"}</p>
    <label class="field">ソース名の接頭辞<input id="prefix" type="text" value="${esc(settings.source_prefix || "Discord")}" /></label>
    <label class="field">希望画質 <span id="quality-label">${esc(settings.prefer_quality ?? 100)}</span><input id="quality" class="slider" type="range" min="0" max="100" value="${esc(settings.prefer_quality ?? 100)}" /></label>
    <p class="hint">画質、受信数、送出先の数、接頭辞、OMT / NDI の切り替えは、次に接続したときから使われます。接続中の送出先は消さずに残します。</p>
    <label class="field">映像の受信スロット<input id="slots" type="number" min="1" max="32" value="${esc(settings.max_video_slots || 8)}" /></label>
    <label class="field">マイクの送出先<input id="audio-slots" type="number" min="0" max="32" value="${esc(settings.audio_slots ?? 8)}" /></label>
    <label class="field">カメラの送出先<input id="camera-slots" type="number" min="0" max="32" value="${esc(settings.camera_slots ?? 8)}" /></label>
    <label class="field">Go Live の送出先<input id="golive-slots" type="number" min="0" max="32" value="${esc(settings.golive_slots ?? 4)}" /></label>
    <label class="field">Go Live の同時視聴数<input id="streams" type="number" min="1" max="32" value="${esc(settings.max_streams || 8)}" /></label>
    <label class="field">無通信で待機表示にする秒数<input id="idle" type="number" min="1" value="${esc(settings.idle_timeout_secs || 5)}" /><span class="hint">受信が止まっても送出先は閉じません。この秒数を過ぎると「待機」になります。</span></label>
  </section>`;
  const limits = `<section class="settings-card" ${tab === "limits" ? "" : "hidden"}>
    <h2>ソースに出すユーザー</h2>
    <p class="hint">許可が空なら、拒否以外の全員を出します。ID は 1 行に 1 つです。</p>
    <label class="field">許可<textarea id="allow">${esc((settings.allow_user_ids || []).join("\n"))}</textarea></label>
    <label class="field">拒否<textarea id="deny">${esc((settings.deny_user_ids || []).join("\n"))}</textarea></label>
  </section>`;
  return `<div class="settings">
    <nav class="settings-nav">
      <button type="button" data-action="settings-tab" data-tab="account" class="${tab === "account" ? "active" : ""}">アカウント</button>
      <button type="button" data-action="settings-tab" data-tab="voice" class="${tab === "voice" ? "active" : ""}">ボイスと送出</button>
      <button type="button" data-action="settings-tab" data-tab="limits" class="${tab === "limits" ? "active" : ""}">ユーザー制限</button>
    </nav>
    <div class="settings-body">
      <button class="close-settings" data-action="close-settings" title="閉じる">×</button>
      ${account}${voice}${limits}
      <div class="unsaved" id="unsaved">
        <span>変更が保存されていません</span>
        <button class="secondary" type="button" data-action="reset-settings">リセット</button>
        <button class="primary" type="button" data-action="save-settings">変更を保存</button>
      </div>
    </div>
  </div>`;
}

function renderSettings() {
  document.getElementById("app").innerHTML = settingsForm();
  state.lastRenderKey = "";
}

function renderGate(message) {
  document.getElementById("app").innerHTML = `<div class="gate"><form class="gate-card" id="gate-form">
    <div class="mark">${iconHome()}</div>
    <h1>Discord OMT Bridge</h1>
    <p class="lede">サーバーとチャンネルを表示し、選んだボイスチャンネルの音声・カメラ・Go Live を LAN へ出します。</p>
    ${message ? `<div class="banner">${esc(message)}</div>` : ""}
    <label class="field">ユーザートークン<input id="token" type="password" autocomplete="off" /></label>
    <button class="primary" type="submit">接続</button>
    <div class="warning">ユーザートークンの利用は Discord の利用規約に違反します。自分の専用アカウント以外では使わないでください。トークンはチャットへ送らず、この端末の設定ファイルにだけ保存します。</div>
  </form></div>`;
  state.lastRenderKey = "";
}

function collectSettings() {
  const lines = (id) => (document.getElementById(id)?.value || "").split(/[\s,]+/).map((item) => item.trim()).filter(Boolean);
  return {
    token: document.getElementById("token")?.value || state.settings?.token || "",
    self_mute: document.getElementById("self-mute")?.checked ?? state.settings.self_mute,
    self_deaf: document.getElementById("self-deaf")?.checked ?? state.settings.self_deaf,
    enable_omt: document.getElementById("enable-omt")?.checked ?? state.settings.enable_omt,
    enable_ndi: state.snapshot.ndi_available && (document.getElementById("enable-ndi")?.checked ?? false),
    source_prefix: document.getElementById("prefix")?.value || "Discord",
    allow_user_ids: document.getElementById("allow") ? lines("allow") : state.settings.allow_user_ids,
    deny_user_ids: document.getElementById("deny") ? lines("deny") : state.settings.deny_user_ids,
    max_streams: Number(document.getElementById("streams")?.value || state.settings.max_streams || 8),
    max_video_slots: Number(document.getElementById("slots")?.value || state.settings.max_video_slots || 8),
    audio_slots: Number(document.getElementById("audio-slots")?.value ?? state.settings.audio_slots ?? 8),
    camera_slots: Number(document.getElementById("camera-slots")?.value ?? state.settings.camera_slots ?? 8),
    golive_slots: Number(document.getElementById("golive-slots")?.value ?? state.settings.golive_slots ?? 4),
    prefer_quality: Number(document.getElementById("quality")?.value || state.settings.prefer_quality || 100),
    idle_timeout_secs: Number(document.getElementById("idle")?.value || state.settings.idle_timeout_secs || 5),
    client_build_number: Number(document.getElementById("build")?.value || 0),
    path: state.settings?.path || "",
  };
}

async function saveCurrentSettings() {
  const settings = collectSettings();
  await invoke("save_settings", { settings });
  state.settings = await invoke("load_settings");
  document.getElementById("unsaved")?.classList.remove("show");
  state.dirty = false;
}

async function toggleVoice(mute, deaf) {
  await invoke("set_voice_flags", { selfMute: mute, selfDeaf: deaf });
  if (state.settings) {
    state.settings.self_mute = mute;
    state.settings.self_deaf = deaf;
  }
}

document.addEventListener("click", async (event) => {
  const target = event.target.closest("[data-action]");
  if (!target) return;
  const action = target.dataset.action;
  try {
    if (action === "home") {
      state.selectedGuildId = null;
      state.selectedChannelId = null;
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "guild") {
      state.selectedGuildId = target.dataset.id;
      state.selectedChannelId = null;
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "channel") {
      state.selectedGuildId = target.dataset.guild;
      state.selectedChannelId = target.dataset.id;
      if (target.dataset.kind === "voice" || target.dataset.kind === "stage") {
        const seat = seatFor(target.dataset.id);
        if (!seat || !seat.connected) {
          await invoke("join_voice", { guildId: target.dataset.guild, channelId: target.dataset.id });
        }
      }
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "pick") {
      state.picked = {
        channelId: target.dataset.channel,
        userId: target.dataset.user,
        name: target.dataset.name,
      };
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "assign-slot") {
      if (!state.picked) return;
      await invoke("assign_slot", {
        slotId: target.dataset.id,
        channelId: state.picked.channelId,
        userId: state.picked.userId,
      });
    } else if (action === "clear-slot") {
      await invoke("clear_slot", { slotId: target.dataset.id });
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "leave") {
      await invoke("leave_voice", { guildId: target.dataset.guild, channelId: target.dataset.channel });
    } else if (action === "toggle-mute") {
      await toggleVoice(!state.snapshot.self_mute, state.snapshot.self_deaf);
    } else if (action === "toggle-deaf") {
      await toggleVoice(state.snapshot.self_mute, !state.snapshot.self_deaf);
    } else if (action === "open-settings") {
      state.settings = await invoke("load_settings");
      state.mode = "settings";
      renderSettings();
    } else if (action === "close-settings") {
      state.mode = "shell";
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "settings-tab") {
      const draft = collectSettings();
      state.settings = { ...state.settings, ...draft };
      state.settingsTab = target.dataset.tab;
      renderSettings();
      if (state.dirty) document.getElementById("unsaved")?.classList.add("show");
    } else if (action === "connect") {
      if (state.mode === "settings" || document.getElementById("token")) {
        await saveCurrentSettings();
      }
      await invoke("connect");
      state.mode = "shell";
      state.lastRenderKey = "";
    } else if (action === "disconnect") {
      await invoke("disconnect");
      state.snapshot = { ...state.snapshot, phase: "disconnected", connected: false, guilds: [], self_user: null, seats: [], slots: [] };
      state.picked = null;
      state.selectedGuildId = null;
      state.selectedChannelId = null;
      state.mode = "shell";
      state.lastRenderKey = "";
      renderShell();
    } else if (action === "save-settings") {
      await saveCurrentSettings();
    } else if (action === "reset-settings") {
      state.settings = await invoke("load_settings");
      renderSettings();
    }
  } catch (err) {
    const message = String(err);
    if (state.mode === "gate") renderGate(message);
    else state.snapshot = { ...state.snapshot, last_error: message };
    state.lastRenderKey = "";
    if (state.mode === "shell") renderShell();
  }
});

document.addEventListener("input", (event) => {
  if (state.mode !== "settings") return;
  if (event.target.id === "quality") {
    const label = document.getElementById("quality-label");
    if (label) label.textContent = event.target.value;
  }
  state.dirty = true;
  document.getElementById("unsaved")?.classList.add("show");
});

document.addEventListener("submit", async (event) => {
  if (event.target.id !== "gate-form") return;
  event.preventDefault();
  try {
    state.settings = await invoke("load_settings");
    state.settings.token = document.getElementById("token").value.trim();
    await invoke("save_settings", { settings: state.settings });
    state.settings = await invoke("load_settings");
    await invoke("connect");
    state.mode = "shell";
    state.lastRenderKey = "";
    renderShell();
  } catch (err) {
    renderGate(String(err));
  }
});

async function tick() {
  try {
    state.snapshot = await invoke("snapshot");
  } catch (err) {
    state.snapshot = { ...state.snapshot, last_error: String(err) };
  }
  if (state.mode !== "shell") return;
  if (state.selectedGuildId && !state.snapshot.guilds.some((guild) => guild.id === state.selectedGuildId)) {
    state.selectedGuildId = state.snapshot.guilds[0]?.id || null;
    state.selectedChannelId = null;
  }
  renderShell();
}

async function boot() {
  if (!window.__TAURI__) {
    document.getElementById("app").innerHTML = `<div class="gate"><div class="gate-card"><h1>Discord OMT Bridge</h1><p class="lede">この画面はデスクトップアプリの中で開きます。<code>cargo run -p discord-omt-desktop</code> で起動してください。</p></div></div>`;
    return;
  }
  try {
    state.settings = await invoke("load_settings");
    state.snapshot = await invoke("snapshot");
    if (state.settings.token.trim()) {
      state.mode = "shell";
      renderShell();
      await invoke("connect");
    } else {
      state.mode = "gate";
      renderGate("");
    }
  } catch (err) {
    state.mode = "gate";
    renderGate(String(err));
  }
  setInterval(tick, 400);
}

boot();
