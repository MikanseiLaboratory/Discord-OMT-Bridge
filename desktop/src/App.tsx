import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { IconGear, IconHash, IconHeadphones, IconHome, IconLeave, IconLog, IconSpeaker } from "./icons";
import {
  type AuthState,
  type AuthStep,
  type Channel,
  type Guild,
  type Output,
  type Settings,
  type Snapshot,
  avatarUrl,
  channelTree,
  colorFromId,
  emptyAuth,
  emptySnapshot,
  findChannel,
  guildIconUrl,
  inTauri,
  initial,
  isAudioOutput,
  kindLabel,
  memberName,
  outputStatus,
  phaseLabel,
  routeOf,
  transportLabel,
  videoInputs,
  voiceCandidates,
} from "./model";

type Mode = "gate" | "shell" | "settings" | "logs";
type PreviewFrame = { user_id: string; name: string; kind: string; width: number; height: number; rgba: string };
type LogLine = { id: number; at_ms: number; level: string; target: string; message: string };
type Picked = { channelId: string; userId: string; name: string } | null;
type AudioDevice = { name: string; host: string; channels: number };

function knownTransport(value: string) {
  if (value === "ndi" || value === "device" || value === "asio") return value;
  return "omt";
}

function messageOf(err: unknown) {
  return err instanceof Error ? err.message : String(err);
}

function Avatar({ id, hash, name, large }: { id: string; hash: string | null; name: string; large?: boolean }) {
  const url = avatarUrl(id, hash, large ? 128 : 64);
  return (
    <span className={large ? "avatar-lg" : "avatar"} style={{ background: colorFromId(id) }}>
      {url ? <img src={url} alt="" /> : initial(name)}
    </span>
  );
}

function Matrix({
  snapshot,
  settings,
  onAdd,
  onRename,
  onTransport,
  onRoute,
  onPlayback,
  onMember,
  onRemove,
}: {
  snapshot: Snapshot;
  settings: Settings;
  onAdd: (name: string, kind: string, transport: string) => Promise<void>;
  onRename: (outputId: string, name: string) => Promise<void>;
  onTransport: (outputId: string, transport: string) => Promise<void>;
  onRoute: (outputId: string, value: string) => Promise<void>;
  onPlayback: (outputId: string, device: string, channelStart: number, channelCount: number) => Promise<void>;
  onMember: (outputId: string, channelId: string, userId: string, volume: number, included: boolean) => Promise<void>;
  onRemove: (output: Output) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState("audio");
  const [transport, setTransport] = useState(snapshot.ndi_available ? "ndi" : "omt");
  const audio = kind === "audio";
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const outputs = settings.outputs || [];
  useEffect(() => {
    let stop = false;
    const pull = async () => {
      try {
        const next = await invoke<AudioDevice[]>("playback_devices");
        if (!stop) setDevices(next);
      } catch {
        if (!stop) setDevices([]);
      }
    };
    void pull();
    const timer = window.setInterval(() => void pull(), 5000);
    return () => {
      stop = true;
      window.clearInterval(timer);
    };
  }, []);
  return (
    <div className="matrix">
      <p className="hint">音声と映像は別の出力です。音声は複数人を混ぜて、人ごとに音量を変えられます。映像は1人のカメラか Go Live で、出し先は OMT か NDI です。Go Live の音声は、その映像の OMT に乗せて送れます。</p>
      <div className="matrix-add">
        <input
          type="text"
          placeholder="出力の名前"
          value={name}
          onChange={(event) => setName(event.target.value)}
          onKeyDown={(event) => {
            if (event.key !== "Enter") return;
            event.preventDefault();
            void onAdd(name, kind, audio ? transport : transport === "device" || transport === "asio" ? "omt" : transport)
              .then(() => setName(""))
              .catch(() => {});
          }}
        />
        <select
          aria-label="出力の種類"
          value={kind}
          onChange={(event) => {
            const next = event.target.value;
            setKind(next);
            if (next !== "audio" && (transport === "device" || transport === "asio")) setTransport("omt");
          }}
        >
          <option value="audio">音声</option>
          <option value="camera">カメラ</option>
          <option value="golive">Go Live</option>
        </select>
        <TransportSelect snapshot={snapshot} audio={audio} value={transport} onChange={setTransport} />
        <button
          className="primary"
          type="button"
          onClick={() => {
            void onAdd(name, kind, audio ? transport : transport === "device" || transport === "asio" ? "omt" : transport)
              .then(() => setName(""))
              .catch(() => {});
          }}
        >
          追加
        </button>
      </div>
      {outputs.length === 0 ? (
        <div className="empty">出力はまだありません。名前と出し先を決めて追加すると、その名前で送出されます。</div>
      ) : (
        <div className="matrix-list">
          {outputs.map((output) => (
            <OutputCard
              key={output.id}
              output={output}
              snapshot={snapshot}
              devices={devices}
              onRename={onRename}
              onTransport={onTransport}
              onRoute={onRoute}
              onPlayback={onPlayback}
              onMember={onMember}
              onRemove={onRemove}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function TransportSelect({
  snapshot,
  audio,
  value,
  onChange,
}: {
  snapshot: Snapshot;
  audio: boolean;
  value: string;
  onChange: (value: string) => void;
}) {
  const current = knownTransport(value);
  return (
    <select value={current} onChange={(event) => onChange(event.target.value)}>
      <option value="ndi" disabled={!snapshot.ndi_available && current !== "ndi"}>
        NDI
      </option>
      <option value="omt">OMT</option>
      {audio || current === "device" ? <option value="device">再生デバイス</option> : null}
      {audio && (snapshot.asio_available || current === "asio") ? (
        <option value="asio" disabled={!snapshot.asio_available}>
          ASIO
        </option>
      ) : null}
    </select>
  );
}

function OutputCard({
  output,
  snapshot,
  devices,
  onRename,
  onTransport,
  onRoute,
  onPlayback,
  onMember,
  onRemove,
}: {
  output: Output;
  snapshot: Snapshot;
  devices: AudioDevice[];
  onRename: (outputId: string, name: string) => Promise<void>;
  onTransport: (outputId: string, transport: string) => Promise<void>;
  onRoute: (outputId: string, value: string) => Promise<void>;
  onPlayback: (outputId: string, device: string, channelStart: number, channelCount: number) => Promise<void>;
  onMember: (outputId: string, channelId: string, userId: string, volume: number, included: boolean) => Promise<void>;
  onRemove: (output: Output) => Promise<void>;
}) {
  const [name, setName] = useState(output.name);
  const [volumes, setVolumes] = useState<Record<string, number>>({});
  useEffect(() => setName(output.name), [output.name]);
  const current = routeOf(output);
  const audio = isAudioOutput(output);
  const transport = knownTransport(output.transport);
  const host = transport === "asio" ? "asio" : "device";
  const choices = devices.filter((device) => device.host === host);
  const members = output.members || [];
  const playback = async (device: string, channelStart: number, channelCount: number) => {
    await onPlayback(output.id, device, channelStart, channelCount);
  };
  const commitVolume = (channelId: string, userId: string) => {
    const key = `${channelId}:${userId}`;
    const volume = volumes[key];
    if (volume === undefined) return;
    void onMember(output.id, channelId, userId, volume, true).finally(() => {
      setVolumes((current) => {
        const next = { ...current };
        delete next[key];
        return next;
      });
    });
  };
  return (
    <div className="source-card">
      <input
        type="text"
        aria-label="出力の名前"
        value={name}
        onChange={(event) => setName(event.target.value)}
        onBlur={() => void onRename(output.id, name)}
      />
      <TransportSelect snapshot={snapshot} audio={audio} value={transport} onChange={(value) => void onTransport(output.id, value)} />
      {transport === "device" || transport === "asio" ? (
        <select aria-label="再生デバイス" value={output.device || ""} onChange={(event) => void playback(event.target.value, output.channel_start || 1, output.channel_count || 2)}>
          {transport === "device" ? <option value="">既定の再生デバイス</option> : <option value="">ASIO デバイスを選択</option>}
          {output.device && !choices.some((device) => device.name === output.device) ? <option value={output.device}>{output.device}</option> : null}
          {choices.map((device) => (
            <option key={`${device.host}:${device.name}`} value={device.name}>
              {device.name}
              {device.channels ? ` (${device.channels}ch)` : ""}
            </option>
          ))}
        </select>
      ) : null}
      {transport === "asio" ? (
        <>
          <input
            className="channel-field"
            type="number"
            min={1}
            aria-label="開始チャンネル"
            title="開始チャンネル"
            value={output.channel_start || 1}
            onChange={(event) => void playback(output.device || "", Number(event.target.value || 1), output.channel_count || 2)}
          />
          <input
            className="channel-field"
            type="number"
            min={1}
            max={64}
            aria-label="チャンネル数"
            title="チャンネル数"
            value={output.channel_count || 2}
            onChange={(event) => void playback(output.device || "", output.channel_start || 1, Number(event.target.value || 2))}
          />
        </>
      ) : null}
      {audio ? (
        <>
          {members.map((member) => {
            const key = `${member.channel_id}:${member.user_id}`;
            const volume = volumes[key] ?? member.volume ?? 100;
            return (
              <div className="member-row" key={key}>
                <span>{memberName(snapshot, member.channel_id, member.user_id)}</span>
                <input
                  type="range"
                  min={0}
                  max={200}
                  aria-label="音量"
                  value={volume}
                  onChange={(event) => setVolumes((current) => ({ ...current, [key]: Number(event.target.value) }))}
                  onPointerUp={() => commitVolume(member.channel_id, member.user_id)}
                  onKeyUp={() => commitVolume(member.channel_id, member.user_id)}
                  onBlur={() => commitVolume(member.channel_id, member.user_id)}
                />
                <span className="volume">{volume}%</span>
                <button className="secondary" type="button" onClick={() => void onMember(output.id, member.channel_id, member.user_id, volume, false)}>
                  外す
                </button>
              </div>
            );
          })}
          <select
            aria-label="人を追加"
            value=""
            onChange={(event) => {
              const [channelId = "", userId = ""] = event.target.value.split(":");
              if (!channelId || !userId) return;
              void onMember(output.id, channelId, userId, 100, true);
            }}
          >
            <option value="">人を追加</option>
            {voiceCandidates(snapshot)
              .filter((candidate) => !members.some((member) => member.channel_id === candidate.channelId && member.user_id === candidate.userId))
              .map((candidate) => (
                <option key={`${candidate.channelId}:${candidate.userId}`} value={`${candidate.channelId}:${candidate.userId}`}>
                  {candidate.label}
                </option>
              ))}
          </select>
        </>
      ) : (
        <select aria-label="入力" value={current} onChange={(event) => void onRoute(output.id, event.target.value)}>
          {videoInputs(snapshot, output).map((option) => (
            <option key={option.value || "none"} value={option.value}>
              {option.label}
            </option>
          ))}
        </select>
      )}
      <div className="meta">
        {transportLabel(transport)} · {audio ? (members.length ? `${members.length}人` : "入力なし") : current ? kindLabel(output.kind) : "入力なし"} · {outputStatus(snapshot, output)}
      </div>
      <button className="secondary" type="button" onClick={() => void onRemove(output)}>
        削除
      </button>
    </div>
  );
}

function Gate({
  auth,
  message,
  onLogin,
  onToken,
  onMfa,
  onSms,
  onBack,
}: {
  auth: AuthState;
  message: string;
  onLogin: () => Promise<void>;
  onToken: (token: string) => Promise<void>;
  onMfa: (code: string) => Promise<void>;
  onSms: () => Promise<void>;
  onBack: () => void;
}) {
  const [token, setToken] = useState("");
  const [code, setCode] = useState("");
  return (
    <div className="gate">
      <form
        className="gate-card"
        onSubmit={(event) => {
          event.preventDefault();
          if (auth.screen === "mfa") void onMfa(code);
          else void onLogin();
        }}
      >
        <div className="mark">
          <IconHome />
        </div>
        <h1>Discord OMT Bridge</h1>
        {auth.screen === "mfa" ? (
          <>
            <p className="lede">
              {auth.method === "sms"
                ? "届いた SMS のコードを入力してください。"
                : "認証アプリの6桁、またはバックアップコードを入力してください。"}
            </p>
            {message ? <div className="banner">{message}</div> : null}
            <label className="field">
              認証コード
              <input value={code} onChange={(event) => setCode(event.target.value)} inputMode="numeric" autoComplete="one-time-code" />
            </label>
            <button className="primary" type="submit">
              確認
            </button>
            <div className="actions">
              {auth.sms ? (
                <button className="secondary" type="button" onClick={() => void onSms()}>
                  SMS を送る
                </button>
              ) : null}
              <button className="secondary" type="button" onClick={onBack}>
                戻る
              </button>
            </div>
          </>
        ) : (
          <>
            <p className="lede">ログインは Discord の画面で一度だけ行います。確認と二段階認証もそこで完了し、終わるとこのアプリに戻ります。パスワードは保存しません。</p>
            {message ? <div className="banner">{message}</div> : null}
            <button className="primary" type="submit">
              ログイン
            </button>
            <details>
              <summary>保存済みのトークンで接続</summary>
              <label className="field">
                トークン
                <input type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} />
              </label>
              <button className="secondary" type="button" onClick={() => void onToken(token)}>
                トークンで接続
              </button>
            </details>
          </>
        )}
        <div className="warning">ユーザートークンで参加する方式は Discord の利用規約に反し、アカウント停止の対象です。自分の専用アカウント以外では使わないでください。</div>
      </form>
    </div>
  );
}

function SettingsScreen({
  initial,
  snapshot,
  onClose,
  onSave,
  onConnect,
  onDisconnect,
}: {
  initial: Settings;
  snapshot: Snapshot;
  onClose: () => void;
  onSave: (settings: Settings) => Promise<void>;
  onConnect: (settings: Settings) => Promise<void>;
  onDisconnect: () => Promise<void>;
}) {
  const [tab, setTab] = useState<"account" | "voice" | "limits">("account");
  const [draft, setDraft] = useState(initial);
  const [allowText, setAllowText] = useState(initial.allow_user_ids.join("\n"));
  const [denyText, setDenyText] = useState(initial.deny_user_ids.join("\n"));
  const [dirty, setDirty] = useState(false);
  const lines = (text: string) => text.split(/[\s,]+/).map((item) => item.trim()).filter(Boolean);
  const collected = (): Settings => ({
    ...draft,
    allow_user_ids: lines(allowText),
    deny_user_ids: lines(denyText),
    outputs: initial.outputs || [],
  });
  const mark = () => setDirty(true);
  return (
    <div className="settings">
      <nav className="settings-nav">
        <button type="button" className={tab === "account" ? "active" : ""} onClick={() => setTab("account")}>
          アカウント
        </button>
        <button type="button" className={tab === "voice" ? "active" : ""} onClick={() => setTab("voice")}>
          ボイスと送出
        </button>
        <button type="button" className={tab === "limits" ? "active" : ""} onClick={() => setTab("limits")}>
          ユーザー制限
        </button>
      </nav>
      <div className="settings-body">
        <button className="close-settings" type="button" title="閉じる" onClick={onClose}>
          ×
        </button>
        <section className="settings-card" hidden={tab !== "account"}>
          <h2>アカウント</h2>
          <p className="hint">最初の画面でメールアドレスとパスワード、必要なら二段階認証でログインします。パスワードは保存しません。ここにあるのは、そのあとこの端末に残したトークンです。</p>
          <div className="warning">トークンはパスワードと同じです。{draft.path || ""} にだけ保存し、ログには出しません。</div>
          <label className="field">
            保存済みトークン
            <input
              type="password"
              autoComplete="off"
              value={draft.token}
              onChange={(event) => {
                mark();
                setDraft({ ...draft, token: event.target.value });
              }}
            />
          </label>
          <label className="field">
            クライアント build number
            <input
              type="number"
              min={0}
              value={draft.client_build_number || 0}
              onChange={(event) => {
                mark();
                setDraft({ ...draft, client_build_number: Number(event.target.value || 0) });
              }}
            />
            <span className="hint">0 のときは起動時に安定版の番号を取ります。変更は再接続後に使われます。</span>
          </label>
          <div className="actions">
            <button className="primary" type="button" onClick={() => void onConnect(collected()).catch(() => {})}>
              接続
            </button>
            <button className="secondary" type="button" onClick={() => void onDisconnect()}>
              切断
            </button>
          </div>
        </section>
        <section className="settings-card" hidden={tab !== "voice"}>
          <h2>ボイスと送出</h2>
          <p className="hint">
            マイクは常にミュートです。スピーカーミュートはホームのボタンです。映像は最高画質で受信し、告知された Go Live は制限なく視聴します。
            {snapshot.ndi_available
              ? "映像の出し先は OMT か NDI です。音声はそれに加えて再生デバイスと ASIO も選べます。"
              : "このビルドには NDI が含まれていません。NDI 6 SDK を入れて起動し直してください。"}
            {snapshot.asio_available ? "" : " ASIO は、Steinberg ASIO SDK を入れて asio 機能付きでビルドしたときだけ選べます。"}
          </p>
          <label className="field">
            映像の受信スロット
            <input
              type="number"
              min={1}
              max={32}
              value={draft.max_video_slots || 8}
              onChange={(event) => {
                mark();
                setDraft({ ...draft, max_video_slots: Number(event.target.value || 8) });
              }}
            />
          </label>
          <label className="field">
            無通信で待機表示にする秒数
            <input
              type="number"
              min={1}
              value={draft.idle_timeout_secs || 5}
              onChange={(event) => {
                mark();
                setDraft({ ...draft, idle_timeout_secs: Number(event.target.value || 5) });
              }}
            />
            <span className="hint">受信が止まっても出力は閉じません。この秒数を過ぎると「待機」になります。受信スロット数は、次に接続したときから使われます。</span>
          </label>
        </section>
        <section className="settings-card" hidden={tab !== "limits"}>
          <h2>ソースに出すユーザー</h2>
          <p className="hint">許可が空なら、拒否以外の全員を出します。ID は 1 行に 1 つです。</p>
          <label className="field">
            許可
            <textarea
              value={allowText}
              onChange={(event) => {
                mark();
                setAllowText(event.target.value);
              }}
            />
          </label>
          <label className="field">
            拒否
            <textarea
              value={denyText}
              onChange={(event) => {
                mark();
                setDenyText(event.target.value);
              }}
            />
          </label>
        </section>
        <div className={dirty ? "unsaved show" : "unsaved"} id="unsaved">
          <span>変更が保存されていません</span>
          <button
            className="secondary"
            type="button"
            onClick={() => {
              setDraft(initial);
              setAllowText(initial.allow_user_ids.join("\n"));
              setDenyText(initial.deny_user_ids.join("\n"));
              setDirty(false);
            }}
          >
            リセット
          </button>
          <button
            className="primary"
            type="button"
            onClick={() => {
              void onSave(collected())
              .then(() => setDirty(false))
              .catch(() => {});
            }}
          >
            変更を保存
          </button>
        </div>
      </div>
    </div>
  );
}

export default function App() {
  const [ready, setReady] = useState(false);
  const [mode, setMode] = useState<Mode>("gate");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot);
  const [auth, setAuth] = useState<AuthState>(emptyAuth);
  const [gateMessage, setGateMessage] = useState("");
  const [selectedGuildId, setSelectedGuildId] = useState<string | null>(null);
  const [selectedChannelId, setSelectedChannelId] = useState<string | null>(null);
  const [picked, setPicked] = useState<Picked>(null);

  const modeRef = useRef(mode);
  modeRef.current = mode;
  const fail = (err: unknown) => {
    const message = messageOf(err);
    if (modeRef.current === "gate") setGateMessage(message);
    else setSnapshot((current) => ({ ...current, last_error: message }));
  };
  const failRef = useRef(fail);
  failRef.current = fail;

  const refreshSettings = async () => {
    const next = await invoke<Settings>("load_settings");
    setSettings(next);
    return next;
  };

  const toggleDeaf = async (deaf: boolean) => {
    await invoke("set_voice_flags", { selfDeaf: deaf });
    setSettings((current) => (current ? { ...current, self_deaf: deaf } : current));
  };

  const addOutput = async (name: string, kind: string, transport: string) => {
    const trimmed = name.trim();
    if (!trimmed) throw new Error("名前を入力してください");
    await invoke("add_output", { name: trimmed, kind, transport });
    await refreshSettings();
  };

  const setMember = async (outputId: string, channelId: string, userId: string, volume: number, included: boolean) => {
    setSettings((current) => {
      if (!current) return current;
      return {
        ...current,
        outputs: current.outputs.map((output) => {
          if (output.id !== outputId) return output;
          const members = (output.members || []).filter((member) => !(member.channel_id === channelId && member.user_id === userId));
          if (included) members.push({ channel_id: channelId, user_id: userId, volume });
          return { ...output, members };
        }),
      };
    });
    try {
      await invoke("set_audio_member", { outputId, channelId, userId, volume, included });
      await refreshSettings();
    } catch (err) {
      await refreshSettings();
      fail(err);
    }
  };

  const renameOutput = async (outputId: string, name: string) => {
    setSettings((current) => {
      if (!current) return current;
      return {
        ...current,
        outputs: current.outputs.map((output) => (output.id === outputId ? { ...output, name: name.trim() } : output)),
      };
    });
    try {
      await invoke("rename_output", { outputId, name });
      await refreshSettings();
    } catch (err) {
      await refreshSettings();
      fail(err);
    }
  };

  const routeOutput = async (outputId: string, value: string) => {
    const [channelId = "0", userId = "0", kind = ""] = value ? value.split(":") : ["0", "0", ""];
    setSettings((current) => {
      if (!current) return current;
      return {
        ...current,
        outputs: current.outputs.map((output) =>
          output.id === outputId
            ? { ...output, channel_id: channelId || "0", user_id: userId || "0", kind: kind || output.kind }
            : output,
        ),
      };
    });
    try {
      await invoke("set_output_route", { outputId, channelId, userId, kind });
      await refreshSettings();
    } catch (err) {
      await refreshSettings();
      fail(err);
    }
  };

  const transportOutput = async (outputId: string, transport: string) => {
    setSettings((current) => {
      if (!current) return current;
      return {
        ...current,
        outputs: current.outputs.map((output) => (output.id === outputId ? { ...output, transport } : output)),
      };
    });
    try {
      await invoke("set_output_transport", { outputId, transport });
      await refreshSettings();
    } catch (err) {
      await refreshSettings();
      fail(err);
    }
  };

  const removeOutput = async (output: Output) => {
    if (!confirm(`「${output.name || "この出力"}」を削除します。LAN 上のソースも閉じます。`)) return;
    await invoke("remove_output", { outputId: output.id });
    await refreshSettings();
  };

  const setPlayback = async (outputId: string, device: string, channelStart: number, channelCount: number) => {
    setSettings((current) => {
      if (!current) return current;
      return {
        ...current,
        outputs: current.outputs.map((output) =>
          output.id === outputId ? { ...output, device, channel_start: channelStart, channel_count: channelCount } : output,
        ),
      };
    });
    try {
      await invoke("set_output_playback", { outputId, device, channelStart, channelCount });
      await refreshSettings();
    } catch (err) {
      await refreshSettings();
      fail(err);
    }
  };

  const openDiscordLogin = async () => {
    setAuth(emptyAuth());
    await invoke("open_discord_login");
    setGateMessage("Discord のログイン画面を開きました。ログインが終わると、この画面に戻って接続します。");
    setMode("gate");
  };

  const connectToken = async (token: string) => {
    const loaded = await refreshSettings();
    const next = { ...loaded, token: token.trim() };
    if (!next.token) throw new Error("トークンを入力してください");
    await invoke("save_settings", { settings: next });
    await refreshSettings();
    await invoke("connect");
    setMode("shell");
  };

  const finishLogin = async (step: AuthStep) => {
    if (step.kind === "token" && step.token) {
      await connectToken(step.token);
      return;
    }
    if (step.kind === "mfa") {
      setAuth({
        screen: "mfa",
        ticket: step.ticket || "",
        method: "totp",
        sms: Boolean(step.sms),
        phone: "",
      });
      setGateMessage("");
      setMode("gate");
      return;
    }
    if (step.kind === "captcha") {
      await openDiscordLogin();
      return;
    }
    if (step.kind === "sms") {
      setAuth((current) => ({ ...current, screen: "mfa", method: "sms", phone: step.phone || "" }));
      setGateMessage(step.phone ? `SMS を ${step.phone} に送りました。` : "SMS を送りました。届いたコードを入力してください。");
      setMode("gate");
    }
  };

  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (err) {
      fail(err);
    }
  };

  useEffect(() => {
    if (!inTauri()) {
      setReady(true);
      return;
    }
    let stop = false;
    let timer = 0;
    const unlistenPromise = listen("discord-login-finished", () => {
      void (async () => {
        try {
          setAuth(emptyAuth());
          await invoke<Settings>("load_settings").then(setSettings);
          await invoke("connect");
          setMode("shell");
        } catch (err) {
          failRef.current(err);
        }
      })();
    });
    const poll = async () => {
      try {
        const next = await invoke<Snapshot>("snapshot");
        if (stop) return;
        setSnapshot(next);
        if (modeRef.current !== "shell") return;
        setSelectedGuildId((current) => {
          if (current && !next.guilds.some((guild) => guild.id === current)) {
            setSelectedChannelId(null);
            return next.guilds[0]?.id || null;
          }
          return current;
        });
      } catch (err) {
        if (!stop) setSnapshot((current) => ({ ...current, last_error: messageOf(err) }));
      }
    };
    void (async () => {
      try {
        const loaded = await invoke<Settings>("load_settings");
        const current = await invoke<Snapshot>("snapshot");
        if (stop) return;
        setSettings(loaded);
        setSnapshot(current);
        if (loaded.token.trim()) {
          setMode("shell");
          await invoke("connect");
        } else {
          setMode("gate");
        }
      } catch (err) {
        if (!stop) {
          setMode("gate");
          setGateMessage(messageOf(err));
        }
      } finally {
        if (!stop) setReady(true);
      }
    })();
    timer = window.setInterval(() => void poll(), 400);
    return () => {
      stop = true;
      window.clearInterval(timer);
      void unlistenPromise.then((unlisten) => unlisten());
    };
    // The login listener and poll are started once for the window lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (!ready) return null;
  if (!inTauri()) {
    return (
      <div className="gate">
        <div className="gate-card">
          <h1>Discord OMT Bridge</h1>
          <p className="lede">
            この画面はデスクトップアプリの中で開きます。<code>npm run tauri dev</code> で起動してください。
          </p>
        </div>
      </div>
    );
  }
  if (!settings) return null;

  if (mode === "gate") {
    return (
      <Gate
        auth={auth}
        message={gateMessage}
        onLogin={openDiscordLogin}
        onToken={(token) => run(() => connectToken(token))}
        onMfa={(code) =>
          run(async () => {
            const step = await invoke<AuthStep>("discord_mfa", {
              ticket: auth.ticket,
              code: code.trim(),
              method: auth.method === "sms" ? "sms" : "totp",
            });
            await finishLogin(step);
          })
        }
        onSms={() =>
          run(async () => {
            const step = await invoke<AuthStep>("discord_sms", { ticket: auth.ticket });
            await finishLogin(step);
          })
        }
        onBack={() => {
          setAuth(emptyAuth());
          setGateMessage("");
        }}
      />
    );
  }

  if (mode === "logs") {
    return <LogScreen onClose={() => setMode("shell")} />;
  }

  if (mode === "settings") {
    return (
      <SettingsScreen
        initial={settings}
        snapshot={snapshot}
        onClose={() => setMode("shell")}
        onSave={async (next) => {
          try {
            await invoke("save_settings", { settings: next });
            await refreshSettings();
          } catch (err) {
            fail(err);
            throw err;
          }
        }}
        onConnect={async (next) => {
          try {
            await invoke("save_settings", { settings: next });
            await refreshSettings();
            await invoke("connect");
            setMode("shell");
          } catch (err) {
            fail(err);
            throw err;
          }
        }}
        onDisconnect={async () => {
          try {
            await invoke("disconnect");
          } catch (err) {
            fail(err);
            return;
          }
          setSnapshot((current) => ({
            ...current,
            phase: "disconnected",
            connected: false,
            guilds: [],
            self_user: null,
            seats: [],
            slots: [],
          }));
          setPicked(null);
          setSelectedGuildId(null);
          setSelectedChannelId(null);
          setMode("shell");
        }}
      />
    );
  }

  const guild = snapshot.guilds.find((item) => item.id === selectedGuildId) || null;
  const channel = guild && selectedChannelId ? guild.channels.find((item) => item.id === selectedChannelId) || null : null;
  const matrix = (
    <Matrix
      snapshot={snapshot}
      settings={settings}
      onAdd={async (name, kind, transport) => {
        try {
          await addOutput(name, kind, transport);
        } catch (err) {
          fail(err);
          throw err;
        }
      }}
      onRename={renameOutput}
      onTransport={transportOutput}
      onRoute={routeOutput}
      onPlayback={setPlayback}
      onMember={setMember}
      onRemove={(output) => run(() => removeOutput(output))}
    />
  );

  return (
    <div className={selectedGuildId ? "shell" : "shell home-layout"}>
      <ServerColumn
        guilds={snapshot.guilds}
        selectedGuildId={selectedGuildId}
        onHome={() => {
          setSelectedGuildId(null);
          setSelectedChannelId(null);
        }}
        onGuild={(id) => {
          setSelectedGuildId(id);
          setSelectedChannelId(null);
        }}
      />
      <ChannelColumn
        guild={guild}
        snapshot={snapshot}
        selectedChannelId={selectedChannelId}
        picked={picked}
        onChannel={(next) =>
          run(async () => {
            setSelectedGuildId(next.guildId);
            setSelectedChannelId(next.channelId);
            if (next.kind === "voice" || next.kind === "stage") {
              const seat = snapshot.seats.find((item) => item.channel_id === next.channelId);
              if (!seat || !seat.connected) {
                await invoke("join_voice", { guildId: next.guildId, channelId: next.channelId });
              }
            }
          })
        }
        onPick={setPicked}
        onLeave={(guildId, channelId) => run(() => invoke("leave_voice", { guildId, channelId }))}
        onToggleDeaf={() => run(() => toggleDeaf(!snapshot.self_deaf))}
        onOpenSettings={() =>
          run(async () => {
            await refreshSettings();
            setMode("settings");
          })
        }
        onOpenLogs={() => setMode("logs")}
      />
      <MainColumn
        guild={guild}
        channel={channel}
        snapshot={snapshot}
        picked={picked}
        matrix={matrix}
        onPick={setPicked}
        onConnect={() => run(() => invoke("connect"))}
        onOpenSettings={() =>
          run(async () => {
            await refreshSettings();
            setMode("settings");
          })
        }
      />
      {selectedGuildId ? (
        <aside className="sources">
          <div className="pane-header">出力</div>
          <div className="source-scroll">{matrix}</div>
        </aside>
      ) : null}
    </div>
  );
}

function ServerColumn({
  guilds,
  selectedGuildId,
  onHome,
  onGuild,
}: {
  guilds: Guild[];
  selectedGuildId: string | null;
  onHome: () => void;
  onGuild: (id: string) => void;
}) {
  return (
    <aside className="servers">
      <div className={selectedGuildId ? "server-wrap" : "server-wrap active"}>
        <button className="server-button home" type="button" title="ホーム" onClick={onHome}>
          <IconHome />
        </button>
      </div>
      <div className="server-separator" />
      <div className="server-scroll">
        {guilds.map((guild) => {
          const icon = guildIconUrl(guild.id, guild.icon);
          return (
            <div key={guild.id} className={guild.id === selectedGuildId ? "server-wrap active" : "server-wrap"}>
              <button
                className="server-button"
                type="button"
                title={guild.name}
                style={{ background: icon ? "#313338" : colorFromId(guild.id) }}
                onClick={() => onGuild(guild.id)}
              >
                {icon ? <img src={icon} alt="" /> : initial(guild.name)}
              </button>
            </div>
          );
        })}
      </div>
    </aside>
  );
}

function ChannelColumn({
  guild,
  snapshot,
  selectedChannelId,
  picked,
  onChannel,
  onPick,
  onLeave,
  onToggleDeaf,
  onOpenSettings,
  onOpenLogs,
}: {
  guild: Guild | null;
  snapshot: Snapshot;
  selectedChannelId: string | null;
  picked: Picked;
  onChannel: (channel: { guildId: string; channelId: string; kind: string }) => void;
  onPick: (picked: Picked) => void;
  onLeave: (guildId: string, channelId: string) => void;
  onToggleDeaf: () => void;
  onOpenSettings: () => void;
  onOpenLogs: () => void;
}) {
  const user = snapshot.self_user;
  const online = snapshot.phase === "online" || snapshot.phase === "voice";
  let body: React.ReactNode = <div className="empty">サーバーを選ぶとチャンネルが表示されます。</div>;
  if (guild) {
    if (!guild.channels.length && snapshot.phase === "connecting") {
      body = <div className="empty">チャンネルを受信しています。</div>;
    } else {
      body = channelTree(guild.channels).map((section) => (
        <div key={section.category?.id || "loose"}>
          {section.category ? <div className="category">{section.category.name}</div> : null}
          {section.channels.map((channel) => (
            <ChannelRow
              key={channel.id}
              guild={guild}
              channel={channel}
              active={selectedChannelId === channel.id}
              joined={snapshot.seats.some((seat) => seat.channel_id === channel.id)}
              picked={picked}
              onChannel={onChannel}
              onPick={onPick}
            />
          ))}
        </div>
      ));
    }
  }
  return (
    <aside className="channels">
      <div className="guild-name">{guild ? guild.name : "ホーム"}</div>
      {snapshot.last_error ? <div className="banner">{snapshot.last_error}</div> : null}
      <div className="channel-scroll">{body}</div>
      <VoiceBar snapshot={snapshot} onLeave={onLeave} onToggleDeaf={onToggleDeaf} />
      <div className="user-panel">
        <div className="avatar-wrap">
          <Avatar id={user ? user.id : "0"} hash={user?.avatar || null} name={user ? user.name : "未接続"} />
          <span className={online ? "presence on" : "presence"} />
        </div>
        <div className="who">
          <div className="name">{user ? user.name : "未接続"}</div>
          <div className="status">{phaseLabel(snapshot.phase)}</div>
        </div>
        <button className={snapshot.self_deaf ? "icon-button toggled" : "icon-button"} type="button" title="スピーカーミュート" onClick={onToggleDeaf}>
          <IconHeadphones deaf={snapshot.self_deaf} />
        </button>
        <button className="icon-button" type="button" title="ログ" onClick={onOpenLogs}>
          <IconLog />
        </button>
        <button className="icon-button" type="button" title="ユーザー設定" onClick={onOpenSettings}>
          <IconGear />
        </button>
      </div>
    </aside>
  );
}

function ChannelRow({
  guild,
  channel,
  active,
  joined,
  picked,
  onChannel,
  onPick,
}: {
  guild: Guild;
  channel: Channel;
  active: boolean;
  joined: boolean;
  picked: Picked;
  onChannel: (channel: { guildId: string; channelId: string; kind: string }) => void;
  onPick: (picked: Picked) => void;
}) {
  const classes = ["channel", active ? "active" : "", joined ? "joined" : ""].filter(Boolean).join(" ");
  return (
    <>
      <button className={classes} type="button" onClick={() => onChannel({ guildId: guild.id, channelId: channel.id, kind: channel.kind })}>
        {channel.kind === "voice" || channel.kind === "stage" ? <IconSpeaker /> : <IconHash />}
        <span>{channel.name}</span>
      </button>
      {(channel.members || []).map((member) => {
        const selected = picked && picked.userId === member.id && picked.channelId === channel.id;
        return (
          <div
            key={member.id}
            className={selected ? "voice-user picked" : "voice-user"}
            onClick={() => onPick({ channelId: channel.id, userId: member.id, name: member.name })}
          >
            <Avatar id={member.id} hash={member.avatar} name={member.name} />
            <span>{member.name}</span>
            {member.live ? <span className="live-pill">LIVE</span> : null}
          </div>
        );
      })}
    </>
  );
}

function VoiceBar({
  snapshot,
  onLeave,
  onToggleDeaf,
}: {
  snapshot: Snapshot;
  onLeave: (guildId: string, channelId: string) => void;
  onToggleDeaf: () => void;
}) {
  if (!snapshot.seats.length) return null;
  return (
    <div className="voice-bar">
      <div>{phaseLabel(snapshot.phase)}</div>
      {snapshot.seats.map((seat) => {
        const found = findChannel(snapshot.guilds, seat.channel_id);
        const name = found ? found.channel.name : seat.channel_id;
        return (
          <div className="seat" key={`${seat.guild_id}:${seat.channel_id}`}>
            <div className="where">
              {name}
              {seat.connected ? "" : " · 切断"}
            </div>
            <button className="icon-button leave" type="button" title="このチャンネルから切断" onClick={() => onLeave(seat.guild_id, seat.channel_id)}>
              <IconLeave />
            </button>
          </div>
        );
      })}
      <div className="row">
        <button className={snapshot.self_deaf ? "icon-button toggled" : "icon-button"} type="button" title="スピーカーミュート" onClick={onToggleDeaf}>
          <IconHeadphones deaf={snapshot.self_deaf} />
        </button>
      </div>
    </div>
  );
}

function ReceivedVideo({ showEmpty = false }: { showEmpty?: boolean }) {
  const [frames, setFrames] = useState<PreviewFrame[]>([]);
  useEffect(() => {
    let stop = false;
    const pull = async () => {
      try {
        const next = await invoke<PreviewFrame[]>("previews");
        if (!stop) setFrames(next);
      } catch {
        if (!stop) setFrames([]);
      }
    };
    void pull();
    const timer = window.setInterval(() => void pull(), 200);
    return () => {
      stop = true;
      window.clearInterval(timer);
    };
  }, []);
  if (frames.length === 0) {
    return showEmpty ? <p className="hint">受信した映像はまだありません。</p> : null;
  }
  return (
    <div className="previews">
      {frames.map((frame) => (
        <PreviewCard key={`${frame.user_id}:${frame.kind}`} frame={frame} />
      ))}
    </div>
  );
}

function PreviewCard({ frame }: { frame: PreviewFrame }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const node = canvas.current;
    if (!node) return;
    const ctx = node.getContext("2d");
    if (!ctx) return;
    const binary = atob(frame.rgba);
    const raw = new Uint8ClampedArray(binary.length);
    for (let index = 0; index < binary.length; index += 1) raw[index] = binary.charCodeAt(index);
    if (raw.length !== frame.width * frame.height * 4) return;
    node.width = frame.width;
    node.height = frame.height;
    ctx.putImageData(new ImageData(raw, frame.width, frame.height), 0, 0);
  }, [frame]);
  const label = frame.kind === "camera" ? "カメラ" : frame.kind === "golive" ? "Go Live" : frame.kind;
  return (
    <div className="preview-card">
      <canvas ref={canvas} />
      <div className="preview-caption">
        {frame.name} の{label}
      </div>
    </div>
  );
}

function MainColumn({
  guild,
  channel,
  snapshot,
  picked,
  matrix,
  onPick,
  onConnect,
  onOpenSettings,
}: {
  guild: Guild | null;
  channel: Channel | null;
  snapshot: Snapshot;
  picked: Picked;
  matrix: ReactNode;
  onPick: (picked: Picked) => void;
  onConnect: () => void;
  onOpenSettings: () => void;
}) {
  if (!guild) {
    return (
      <main className="main">
        <div className="pane-header">ホーム</div>
        <div className="home-body">
          <h2>Discord OMT Bridge</h2>
          <p>
            左のサーバーからボイスチャンネルを選ぶと、その通話が席に追加されます。複数のチャンネルへ同時に参加できます。出力を追加し、名前と入力にする人を決めると、その人の音声や映像が OMT / NDI に流れます。カメラや Go Live を切っても出力は残ります。テキストは送受信しません。
          </p>
          <p>状態: {phaseLabel(snapshot.phase)}</p>
          {snapshot.phase === "disconnected" ? (
            <div className="actions">
              <button className="primary" type="button" onClick={onConnect}>
                接続
              </button>
              <button className="secondary" type="button" onClick={onOpenSettings}>
                トークンと設定
              </button>
            </div>
          ) : null}
          <h2>受信した映像</h2>
          <ReceivedVideo />
          <h2>出力</h2>
          {matrix}
        </div>
      </main>
    );
  }
  if (!channel) {
    return (
      <main className="main">
        <div className="pane-header">{guild.name}</div>
        <div className="empty">チャンネルを選択してください。未参加のボイスチャンネルを選ぶと、その通話が席に追加されます。</div>
      </main>
    );
  }
  const icon = channel.kind === "voice" || channel.kind === "stage" ? <IconSpeaker /> : <IconHash />;
  if (channel.kind === "text") {
    return (
      <main className="main">
        <div className="pane-header">
          {icon} {channel.name}
        </div>
        <div className="empty">このアプリはテキストチャンネルの読み書きをしません。映像と音声はボイスチャンネルから受けます。</div>
      </main>
    );
  }
  return (
    <main className="main">
      <div className="pane-header">
        {icon} {channel.name}
      </div>
      <div className="stage">
        <h2>受信した映像</h2>
        <ReceivedVideo showEmpty />
        <div className="tiles">
          {(channel.members || []).length ? (
            channel.members.map((member) => {
              const badges = [member.camera ? "カメラ" : "", member.live ? "LIVE" : "", member.mute ? "ミュート" : ""].filter(Boolean).join(" · ");
              const selected = picked && picked.userId === member.id && picked.channelId === channel.id;
              return (
                <button
                  key={member.id}
                  className={selected ? "tile picked" : "tile"}
                  type="button"
                  onClick={() => onPick({ channelId: channel.id, userId: member.id, name: member.name })}
                >
                  <Avatar id={member.id} hash={member.avatar} name={member.name} large />
                  <div className="caption">
                    <span>{member.name}</span>
                    {badges ? <span>{badges}</span> : null}
                  </div>
                </button>
              );
            })
          ) : (
            <div className="empty">このチャンネルには、まだ誰も表示されていません。</div>
          )}
        </div>
      </div>
    </main>
  );
}

function LogScreen({ onClose }: { onClose: () => void }) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    let stop = false;
    const pull = async () => {
      try {
        const next = await invoke<LogLine[]>("recent_logs");
        if (!stop) setLines(next);
      } catch {
        // Keep the lines already on screen if a poll fails.
      }
    };
    void pull();
    const timer = window.setInterval(() => void pull(), 500);
    return () => {
      stop = true;
      window.clearInterval(timer);
    };
  }, []);
  useEffect(() => {
    const node = scroller.current;
    if (!node || !stick.current) return;
    node.scrollTop = node.scrollHeight;
  }, [lines]);
  const stamp = (at: number) => (at ? new Date(at).toLocaleTimeString("ja-JP", { hour12: false }) : "");
  return (
    <div className="log-view">
      <div className="log-toolbar">
        <h1>ログ</h1>
        <button className="secondary" type="button" onClick={onClose}>
          戻る
        </button>
      </div>
      <div
        className="log-scroll"
        ref={scroller}
        onScroll={(event) => {
          const node = event.currentTarget;
          stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
        }}
      >
        {lines.length === 0 ? <div className="empty">まだログはありません。</div> : null}
        {lines.map((line) => (
          <div key={line.id} className={`log-line ${line.level.toLowerCase()}`}>
            <span>{stamp(line.at_ms)}</span> <span>{line.level.toUpperCase()}</span> <span>{line.message}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
