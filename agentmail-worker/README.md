# SSH ゲートウェイ, クライアント, 通知の設定

## メールホスト

[メールコンテナ](../agentmail-mail-lab/README.md) を起動しておく. ホストでは SSH サーバと, ホームが `/home/agentmail` の専用ユーザが必要. ビルドは各実行環境向けに行う.

```sh
cd agentmail-worker
cargo build --release --locked --bins
```

以下の配置ではリポジトリを `/path/to/agentmail` とする. Linux 向けのビルド済みファイルを使い, 管理操作はメールホストで行う.

1. `/home/agentmail/agentmail-ssh-lab/` に `install-vm-ssh-lab.sh`, `start-vm-mail.sh`, `agentmail`, `agentmail-ssh` を置く. 専用ユーザが所有し, バイナリに実行権限を付ける.
2. 開発環境に専用の Ed25519 鍵を作る. 公開鍵を同じディレクトリの `development.pub` に置き, 専用ユーザで `sh /home/agentmail/agentmail-ssh-lab/install-vm-ssh-lab.sh` を実行する. これは開発環境の初期接続に使う `beta` を登録する.
3. Mac にも別の Ed25519 鍵を作る. 公開鍵をメールホストにコピーし, 専用ユーザで同じスクリプトに `--register-mac /absolute/path/to/mac.pub` を付けて実行する. これは Mac の初期接続に使う `alpha` を登録する.
4. 専用ユーザで `sh /path/to/agentmail/agentmail-worker/install-vm-sessions.sh /absolute/path/to/linux/target/release` を実行する. これで会話の登録を管理する `agentmail-admin` が入る.
5. [受信箱の管理](../agentmail-mail-lab/README.md#受信箱の追加) で使う `manage-vm-mailboxes.sh`, `entrypoint.sh`, Linux 向けの `agentmail-mailboxes` を `/home/agentmail/agentmail-mailboxes-lab/` に置き, 専用ユーザで `sh /home/agentmail/agentmail-mailboxes-lab/manage-vm-mailboxes.sh install` を実行する.

通知を使う場合は, `agentmail-worker` と `agentmail-notify` の Linux バイナリを `install-vm-notifications.sh` と同じディレクトリに置く. `CODEX_HOST_HOME` には, 開発コンテナのホームへバインドマウントしたホスト側のディレクトリを指定する. 次のコマンドを管理者で実行する.

```sh
sudo sh install-vm-notifications.sh /absolute/path/to/agentmail.conf
```

## Mac と開発コンテナ

`agentmail.conf.example` を各マシンの `agentmail.conf` にコピーし, 使用する側の設定を埋める. パスは絶対パスで, 引用符や `$HOME` の展開は不要. `*_SSH_KEY` には手元で作成した初期接続用の秘密鍵のパスを指定する. パスフレーズ付きの鍵は実行前に SSH エージェントに読み込む.

`*_KNOWN_HOSTS` には接続先の確認済みの Ed25519 ホスト鍵を入れる. 鍵の指紋はメールホストで確認する. SSH ポートが 22 以外の場合は, known_hosts のホスト欄を `[HOST]:PORT` にする. SSH は BatchMode と厳密なホスト鍵検証で接続する.

Mac 上では次を実行する.

```sh
cd agentmail-worker
cargo build --release --locked --bin agentmail-ssh --bin agentmail-hook --bin agentmail-install-claude-hooks
sh install-client.sh mac ../agentmail.conf
```

開発コンテナでは, その環境向けに `agentmail-ssh` と `agentmail-hook` をビルドし, `sh install-client.sh development ../agentmail.conf` を実行する. どちらもビルド出力が `target/release` 以外なら, そのディレクトリを第3引数に指定する. コマンドが PATH に出ない場合は `$HOME/.local/bin/agentmail` で実行できる.

## 会話登録と通知

対象の会話で `agentmail prepare-session IDENTITY` を実行する. 初回は表示される案内に従って, メールホストの管理者が受信箱の作成と `bind` を行う. 別の会話へ引き継ぐ際は `rebind` を使う. 対象の会話で `agentmail whoami` を実行して登録を確認する.

Mac では `$HOME/.local/bin/agentmail-install-claude-hooks` で対象の設定を確認し, `--apply` を付けて `~/.claude/settings.json` に PostToolUse と Stop のフックを追加する. 通知を受ける間は Mac のターミナルで `agentmail watch-notifications` を動かす. 会話ごとの一時停止と再開には, 対象の会話で `agentmail watch-notifications --pause` または `--resume` を使う.

Codex では `~/.codex/hooks.json` の PostToolUse と Stop に, 次のコマンドをそれぞれ登録する. 実際に設定する際はパスを絶対パスに置き換え, Codex からフックを承認する.

```sh
"$HOME/.local/lib/agentmail/agentmail-hook" --codex-registered "$HOME/.local/lib/agentmail/agentmail-ssh" "$HOME/.agentmail-notify/notify.sock" "$HOME/.local/share/agentmail/notification-state"
"$HOME/.local/lib/agentmail/agentmail-hook" --stop --codex-registered "$HOME/.local/lib/agentmail/agentmail-ssh" "$HOME/.agentmail-notify/notify.sock" "$HOME/.local/share/agentmail/notification-state"
```

フックは登録済みの会話で新着の差出人, 件名, 未読件数を通知する. 本文は `agentmail read UID` で読む.
