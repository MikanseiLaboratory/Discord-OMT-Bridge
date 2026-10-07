# Discord-OMT-Bridge

Discord のボイスチャンネルに参加し、ユーザーごとのマイク音声・カメラ映像・Go Live を LAN 上の NDI と Open Media Transport (OMT) へ出します。

## 重要な注意

ユーザートークンでクライアントとして参加する方式です。これは Discord の利用規約に反し、アカウント停止の対象になります。専用アカウント以外では使わないでください。トークンは `DISCORD_TOKEN` 環境変数だけで渡し、ファイルやログに書かないでください。

公式の Bot API、Social SDK、RPC ではギルド VC のカメラと Go Live を受け取れません。暗号化は公式の DAVE 実装 (`davey`) を使い、映像の購読だけがユーザークライアントの手順です。

## 構成

- `egress` — NDI (`grafton-ndi`、feature `ndi`) と OMT (`openmediatransport`) への送出
- `discord-user-gateway` — 参加と Go Live 視聴に必要な Gateway だけ
- `discord-media` — Voice / Go Live の WebRTC、DAVE、Opus、H.264
- `discord-omt-bridge` — 設定、送出先への割り当て、複数チャンネル、再接続と監視ログ
- `discord-omt-desktop` — Tauri のウィンドウ。コマンドから同じ Rust の処理を呼ぶだけです

カメラは通常の voice 接続、Go Live は別の voice 接続です。接続時に `Discord / Audio / 1`、`Discord / Camera / 1`、`Discord / GoLive / 1` のような送出先をあらかじめ作り、ユーザーの音声や映像をそこへ当てます。カメラや Go Live を切っても送出先は残し、同じ人が戻したら同じ送出先へ流します。

## デスクトップ

サーバー一覧、チャンネル参加、ミュート、送出設定は Tauri のウィンドウから操作します。見た目は Discord の暗い三列レイアウト（サーバー、チャンネル、通話）に合わせています。

```sh
cargo run -p discord-omt-desktop
```

トークンはウィンドウの初回画面で入力します。保存先は `%APPDATA%\discord-omt-bridge\settings.toml` で、ログには出しません。NDI を画面から有効にするには、NDI 6 SDK を入れたうえで `cargo run -p discord-omt-desktop --features ndi` とビルドします。

テキストチャンネルは一覧に出しますが、読み書きはしません。未参加のボイスチャンネルを選ぶと席が追加され、参加中のチャンネルをもう一度選んでも他の席は切れません。席ごとに切断できます。メンバーを選んでから右の送出先を押すと、その人をその送出先へ固定します。

アカウントのボイス状態は、最後に参加したチャンネルが公式の参加先です。それより前のチャンネルの受信は、サーバがその接続を切るまで維持します。切られた席は「切断」と表示され、もう一度選ぶと入り直します。送出先の数、接頭辞、OMT / NDI の切り替えは次の接続から使われます。

## ビルド

OMT のみ:

```sh
cargo build -p discord-omt-bridge
```

NDI 6 SDK が入っている場合:

```sh
cargo build -p discord-omt-bridge --features ndi
```

Discord なしで送出を確認する:

```sh
cargo run -p egress --bin colorbar -- 10
```

または:

```sh
cargo run -p discord-omt-bridge -- colorbar --seconds 10
```

## 実行

`config.example.toml` を `config.toml` にコピーし、ギルドとチャンネルの ID を入れます。

```sh
set DISCORD_TOKEN=...
cargo run -p discord-omt-bridge -- run --config config.toml
```

`guild_id` と `channel_id` に加え、`[[channels]]` で他のボイスチャンネルも同時に参加できます。上のコマンドが GUI なしの本体です。デスクトップ版は同じ処理をコマンド経由で呼びます。OBS では OMT プラグイン、または NDI ソースとして `Discord / Audio|Camera|GoLive / 1` を受信します。無通信でも送出先は閉じず、5 秒ごとに名前、割り当て、フレーム数、欠落、FPS をログへ出します。

H.265 と AV1 はデコード対象にしません。届いた場合は捨てます。
