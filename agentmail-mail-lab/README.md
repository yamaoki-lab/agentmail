# エージェント間メールの疎通検証

Debian VM 上で rootless Podman の検証コンテナを起動し, Postfix から LMTP 経由で Dovecot の Maildir に届ける. 2 つのアドレス間でメールの送信と返信を行い, IMAP 経由で本文を読み取って既読にする.

## 実行場所

このディレクトリをメールホストにコピーし, 専用ユーザ `agentmail` が読み取れる場所へ配置する.

ホストに Podman と専用ユーザが用意できたら, そのユーザで実行する. 引数には Linux 向けにビルドした `agentmail` と `agentmail-mailboxes` のあるディレクトリを指定する.

```sh
cd /path/to/agentmail-mail-lab
sh run-lab.sh /path/to/linux/target/release
```

検証スクリプトでは, systemd ユーザセッションに依存しないよう, ビルド時に chroot 分離を, 実行時にコンテナ専用 cgroup を作成しない設定をコマンドへ指定している.

ビルド時は Debian ベースイメージとパッケージを取得するためにネットワーク接続が必要である. 一方, 実行時のコンテナにはネットワークを接続せず, SMTP と IMAP はコンテナ内のループバックのみでテストする. テストの認証情報は起動時に生成する. Maildir は専用の名前付きボリュームに保存する.

## テストの再実行と調査

VM 上で, コンテナを起動した専用ユーザで実行する.

```sh
podman --cgroup-manager=cgroupfs exec agentmail-mail-lab python3 /opt/mail-lab/smoke.py
podman --cgroup-manager=cgroupfs logs --tail 80 agentmail-mail-lab
podman --cgroup-manager=cgroupfs exec -it agentmail-mail-lab bash
```

疎通テストでは, メールの送信, 返信, IMAP での既読操作, 宛先不明時の拒否, および Maildir への保存を確認する. また, ログイン後の IMAP CAPABILITY 応答で NOTIFY の有無も表示する. SSH 接続と通知は [クライアントの設定](../agentmail-worker/README.md)を参照する.

## IMAP NOTIFY の新着イベント検証

VM 上に `notify-probe.py` をコピーし, 起動済みの検証コンテナで実行する. イメージの再ビルドは不要.

```sh
sudo -iu agentmail podman --cgroup-manager=cgroupfs exec -i \
  agentmail-mail-lab python3 - < /tmp/agentmail-notify-probe.py
```

IMAP NOTIFY で受信箱の監視を始めたあとにテストメールを 1 通送信する. IMAP に追加コマンドを送る前に新着の STATUS を受け取り, その後に UID と Message-ID が一致することを別の IMAP 接続で確認する.

## 受信箱の追加

VM 側で既存のメールコンテナを起動し, [SSH ゲートウェイの設定](../agentmail-worker/README.md#メールホスト)に従って管理スクリプトを配置する. そのユーザで `sh manage-vm-mailboxes.sh install` を一度実行する.

```sh
sh manage-vm-mailboxes.sh identity list
sh manage-vm-mailboxes.sh identity create lan-check
```

`create` はメールアドレスを既存の Maildir ボリューム内に登録し, Postfix と Dovecot の設定に反映する. SMTP で宛先が受け付けられ, IMAP で受信箱を開けたときに `READY` を表示する. コマンドを再実行した場合は同じ受信箱を再利用する. また, 登録情報はコンテナの起動時に再適用される. 会話への割り当てと引き継ぎには `agentmail-admin` を使う.
