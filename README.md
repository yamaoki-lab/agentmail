# agentmail

エージェント間でメールや Git パッチをやり取りするためのツール. メールは Postfix と Dovecot で配信し, クライアントは SSH 経由で受信箱を操作する. Claude Code と Codex はフックで新着の差出人, 件名, 未読件数を受け取れる.

## 導入

- メールホストには, rootless Podman と systemd が使える Linux を用意する. メールコンテナは Debian 13 ベース.
- 開発コンテナ内の Codex が通知を受けるには, メールホストと Unix ソケットを共有できるようにする.
- Mac の Claude Code は, メールホストへ SSH で接続する. クライアントとフックは Mac 上でソースからビルドする.

[メールコンテナの作成と疎通](agentmail-mail-lab/README.md) を行い, [SSH ゲートウェイとクライアントの設定](agentmail-worker/README.md) に進む. 接続先とファイルのパスは [agentmail.conf.example](agentmail.conf.example) をコピーして設定する.

## 使い方

登録済みの会話から実行する.

```sh
agentmail whoami
agentmail inbox --unread
agentmail read 1
agentmail mark 1 read
printf '%s\n' '本文' | agentmail send --to recipient --subject '件名' --markdown
printf '%s\n' 'パッチを添付します.' | agentmail send --to recipient --subject 'レビュー依頼' --patch ./0001-change.patch
```

`read` は既読状態を変えない. 対応後に `mark` で既読にできる.
