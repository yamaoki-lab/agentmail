# エージェント間メールの疎通検証

Debian VM 上で rootless Podman の検証コンテナを起動し、Postfix から LMTP 経由で Dovecot の Maildir に届ける。2アドレス間で送信と返信を行い、IMAP から本文を読んで既読にする。

## 実行場所

このディレクトリをメールホストにコピーし、専用ユーザー `agentmail` が読める場所へ置く。

ホストに Podman と専用ユーザーが用意できたら、そのユーザーで実行する。引数には Linux 向けにビルドした `agentmail` と `agentmail-mailboxes` のあるディレクトリを指定する。

```sh
cd /path/to/agentmail-mail-lab
sh run-lab.sh /path/to/linux/target/release
```

検証スクリプトは systemd ユーザーセッションを必要としないよう、ビルド時は chroot 分離、実行時はコンテナ専用 cgroup を作らない設定をコマンドに指定する。

ビルドには Debian ベースイメージとパッケージを取得するネットワークが必要。実行するコンテナにはネットワークを付けず、SMTP と IMAP はコンテナ内ループバックだけでテストする。テストの認証情報は起動時に生成する。Maildir は専用の名前付きボリュームに保存する。

## テストの再実行と調査

VM 上で、コンテナを起動した専用ユーザーで実行する。

```sh
podman --cgroup-manager=cgroupfs exec agentmail-mail-lab python3 /opt/mail-lab/smoke.py
podman --cgroup-manager=cgroupfs logs --tail 80 agentmail-mail-lab
podman --cgroup-manager=cgroupfs exec -it agentmail-mail-lab bash
```

疎通テストは送信・返信・IMAP の既読操作・宛先不明の拒否・Maildir への保存を確認する。ログイン後の IMAP CAPABILITY で NOTIFY の有無も表示する。外部クライアント向けの SSH 入口、通知ワーカー、運用用 identity は次の段階。

## IMAP NOTIFY の新着イベント検証

VM 上に `notify-probe.py` をコピーし、起動済みの検証コンテナで実行する。イメージの再ビルドは不要。

```sh
sudo -iu agentmail podman --cgroup-manager=cgroupfs exec -i \
  agentmail-mail-lab python3 - < /tmp/agentmail-notify-probe.py
```

受信箱を購読したあとにテストメールを1通送る。IMAP に追加コマンドを送る前に新着の STATUS を受け取り、その後に UID と Message-ID が一致することを別の IMAP 接続で確認する。
## 受信箱の追加

VM 側で既存のメールコンテナを起動し、配布アーカイブの `agentmail-mailboxes-lab/` を `agentmail` ユーザーのホームに展開する。そのユーザーで `sh manage-vm-mailboxes.sh install` を一度実行する。

```sh
sh manage-vm-mailboxes.sh identity list
sh manage-vm-mailboxes.sh identity create lan-check
```

`create` は住所を既存 Maildir ボリューム内に登録し、Postfix と Dovecot に反映する。SMTP が宛先を受け付け、IMAP が受信箱を開けたときに `READY` を表示する。再実行は同じ受信箱を再利用する。登録情報は起動時に再適用する。現段階のリモート用 SSH 鍵は引き続き `alpha`・`beta` に固定され、セッションへの割り当てと引き継ぎは後続。
