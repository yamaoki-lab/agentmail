#!/bin/bash
set -euo pipefail

install -d -m 0755 /data
install -d -m 0700 /run/agentmail-lab
/opt/mail-lab/agentmail-mailboxes sync

doveconf -n >/dev/null
postfix check
dovecot -F &
dovecot_pid=$!
postfix start-fg &
postfix_pid=$!

stop() {
    kill -TERM "$dovecot_pid" "$postfix_pid" 2>/dev/null || true
    wait "$dovecot_pid" "$postfix_pid" 2>/dev/null || true
}
trap 'stop; exit 0' TERM INT
status=0
wait -n "$dovecot_pid" "$postfix_pid" || status=$?
stop
exit "$status"
