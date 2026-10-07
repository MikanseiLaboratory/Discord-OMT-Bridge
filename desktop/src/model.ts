export type AudioMember = {
  channel_id: string;
  user_id: string;
  volume: number;
};

export type Output = {
  id: string;
  name: string;
  kind: string;
  channel_id: string;
  user_id: string;
  transport: string;
  device: string;
  channel_start: number;
  channel_count: number;
  members: AudioMember[];
};

export type Settings = {
  token: string;
  self_deaf: boolean;
  allow_user_ids: string[];
  deny_user_ids: string[];
  max_video_slots: number;
  idle_timeout_secs: number;
  client_build_number: number;
  outputs: Output[];
  path: string;
};

export type Member = {
  id: string;
  name: string;
  avatar: string | null;
  camera: boolean;
  live: boolean;
  mute: boolean;
  deaf: boolean;
};

export type Channel = {
  id: string;
  name: string;
  kind: string;
  position: number;
  parent_id: string | null;
  members: Member[];
};

export type Guild = {
  id: string;
  name: string;
  icon: string | null;
  channels: Channel[];
};

export type Seat = {
  guild_id: string;
  channel_id: string;
  connected: boolean;
  focused: boolean;
};

export type Slot = {
  id: string;
  name: string;
  channel_id: string | null;
  user_id: string | null;
  assigned_name: string;
  live: boolean;
};

export type User = {
  id: string;
  name: string;
  avatar: string | null;
};

export type Snapshot = {
  phase: string;
  connected: boolean;
  self_user: User | null;
  guilds: Guild[];
  seats: Seat[];
  slots: Slot[];
  last_error: string | null;
  ndi_available: boolean;
  asio_available: boolean;
  self_deaf: boolean;
};

export type AuthStep = {
  kind: string;
  token?: string;
  ticket?: string;
  totp?: boolean;
  sms?: boolean;
  backup?: boolean;
  phone?: string;
};

export type AuthState = {
  screen: "password" | "mfa";
  method: "totp" | "sms";
  ticket: string;
  sms: boolean;
  phone: string;
};

export function emptyAuth(): AuthState {
  return { screen: "password", method: "totp", ticket: "", sms: false, phone: "" };
}

export function emptySnapshot(): Snapshot {
  return {
    phase: "disconnected",
    connected: false,
    self_user: null,
    guilds: [],
    seats: [],
    slots: [],
    last_error: null,
    ndi_available: false,
    asio_available: false,
    self_deaf: false,
  };
}

export function phaseLabel(phase: string) {
  switch (phase) {
    case "connecting":
      return "接続しています";
    case "online":
      return "オンライン";
    case "voice":
      return "ボイス接続";
    case "reconnecting":
      return "再接続しています";
    default:
      return "オフライン";
  }
}

export function kindLabel(kind: string) {
  if (kind === "Audio" || kind === "audio") return "音声";
  if (kind === "Camera" || kind === "camera") return "カメラ";
  if (kind === "GoLive" || kind === "golive") return "Go Live";
  return kind;
}

export function kindSlug(kind: string) {
  if (kind === "Camera" || kind === "camera") return "camera";
  if (kind === "GoLive" || kind === "golive") return "golive";
  return "audio";
}

export function transportLabel(transport: string) {
  if (transport === "ndi") return "NDI";
  if (transport === "device") return "再生デバイス";
  if (transport === "asio") return "ASIO";
  return "OMT";
}

export function colorFromId(id: string) {
  const colors = ["#5865f2", "#57f287", "#fee75c", "#eb459e", "#ed4245", "#3ba55c", "#faa61a"];
  let hash = 0;
  for (const ch of String(id)) hash = (hash * 33 + ch.charCodeAt(0)) >>> 0;
  return colors[hash % colors.length];
}

export function avatarUrl(id: string, hash: string | null, size: number) {
  if (!hash) return "";
  const ext = hash.startsWith("a_") ? "gif" : "png";
  return `https://cdn.discordapp.com/avatars/${id}/${hash}.${ext}?size=${size}`;
}

export function guildIconUrl(id: string, icon: string | null) {
  if (!icon) return "";
  const ext = icon.startsWith("a_") ? "gif" : "png";
  return `https://cdn.discordapp.com/icons/${id}/${icon}.${ext}?size=96`;
}

export function initial(name: string) {
  const text = (name || "?").trim();
  return Array.from(text)[0] || "?";
}

export function findChannel(guilds: Guild[], channelId: string) {
  for (const guild of guilds) {
    const channel = guild.channels.find((item) => item.id === channelId);
    if (channel) return { guild, channel };
  }
  return null;
}

export function channelTree(channels: Channel[]) {
  const visible = channels.filter((channel) => channel.kind !== "other");
  const byPos = (a: Channel, b: Channel) => a.position - b.position || a.name.localeCompare(b.name, "ja");
  const categories = visible.filter((channel) => channel.kind === "category").sort(byPos);
  const rest = visible.filter((channel) => channel.kind !== "category");
  const sections: { category: Channel | null; channels: Channel[] }[] = [];
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

export function routeOf(output: Output) {
  if (!output.user_id || output.user_id === "0" || !output.channel_id || output.channel_id === "0") return "";
  return `${output.channel_id}:${output.user_id}:${kindSlug(output.kind)}`;
}

const VIDEO_KINDS = [
  ["camera", "カメラ"],
  ["golive", "Go Live"],
] as const;

export function isAudioOutput(output: Output) {
  return kindSlug(output.kind) === "audio";
}

export function videoInputs(snapshot: Snapshot, output: Output) {
  const options = [{ value: "", label: "なし" }];
  const seen = new Set([""]);
  for (const seat of snapshot.seats || []) {
    const found = findChannel(snapshot.guilds, seat.channel_id);
    const channelName = found ? found.channel.name : "ボイスチャンネル";
    for (const member of found?.channel.members || []) {
      for (const [kind, label] of VIDEO_KINDS) {
        const value = `${seat.channel_id}:${member.id}:${kind}`;
        if (seen.has(value)) continue;
        seen.add(value);
        options.push({ value, label: `${member.name} の${label}（${channelName}）` });
      }
    }
  }
  const current = routeOf(output);
  if (current && !seen.has(current) && !isAudioOutput(output)) {
    const slot = snapshot.slots.find((item) => item.id === output.id);
    const who = slot && slot.assigned_name && slot.assigned_name !== "取得中" ? slot.assigned_name : "取得中";
    options.push({ value: current, label: `${who} の${kindLabel(output.kind)}` });
  }
  return options;
}

export function voiceCandidates(snapshot: Snapshot) {
  const options: { channelId: string; userId: string; label: string }[] = [];
  const seen = new Set<string>();
  for (const seat of snapshot.seats || []) {
    const found = findChannel(snapshot.guilds, seat.channel_id);
    const channelName = found ? found.channel.name : "ボイスチャンネル";
    for (const member of found?.channel.members || []) {
      const key = `${seat.channel_id}:${member.id}`;
      if (seen.has(key)) continue;
      seen.add(key);
      options.push({ channelId: seat.channel_id, userId: member.id, label: `${member.name}（${channelName}）` });
    }
  }
  return options;
}

export function memberName(snapshot: Snapshot, channelId: string, userId: string) {
  const found = findChannel(snapshot.guilds, channelId);
  const member = found?.channel.members.find((item) => item.id === userId);
  if (member?.name) return member.name;
  const slot = snapshot.slots.find((item) => item.user_id === userId && item.assigned_name);
  return slot?.assigned_name || userId;
}

export function outputStatus(snapshot: Snapshot, output: Output) {
  if (!snapshot.connected) return "未接続";
  const slot = snapshot.slots.find((item) => item.id === output.id);
  return slot && slot.live ? "受信中" : "待機";
}

export function inTauri() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}
