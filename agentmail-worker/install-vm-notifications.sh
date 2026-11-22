#!/bin/sh
set -eu
source_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd)
. "$source_dir/deployment-config.sh"

if [ "$(id -u)" -ne 0 ]; then
    echo 'Run as root on the mail VM.' >&2
    exit 1
fi
stage_only=false
if [ "${1:-}" = --stage ]; then
    stage_only=true
    shift
fi
if [ "$#" -gt 1 ]; then
    echo 'Usage: install-vm-notifications.sh [--stage] [CONFIG]' >&2
    exit 2
fi
environment=/etc/agentmail/host.env
if [ "$#" -eq 1 ]; then
    read_deployment_config "$1"
    socket_environment >/dev/null
    if [ ! -d "$CODEX_HOST_HOME" ]; then
        echo 'CODEX_HOST_HOME must exist on this host' >&2
        exit 1
    fi
elif [ ! -r "$environment" ]; then
    echo 'Supply the deployment configuration for the initial installation' >&2
    exit 2
fi
cd "$source_dir"
unit_dir=/etc/systemd/system
if [ -f "$unit_dir/agentmail-notify.service" ] &&
   ! systemctl is-active --quiet agentmail-mail.service; then
    # The live Podman namespace may still belong to the previous notify unit.
    stage_only=true
fi
test -x /home/agentmail/agentmail-ssh-lab/agentmail-admin
test -r /home/agentmail/agentmail-ssh-lab/start-vm-mail.sh
test -x ./agentmail-worker
test -x ./agentmail-notify
destination=/usr/local/lib/agentmail-notify
install -d -m 755 "$destination"
staged=$(mktemp -d "$destination/.install.XXXXXXXX")
environment_stage=
trap 'rm -rf -- "$staged"; if [ -n "$environment_stage" ]; then rm -f -- "$environment_stage"; fi' EXIT
install -m 755 ./agentmail-worker ./agentmail-notify "$staged/"
"$staged/agentmail-notify" --help >/dev/null
if [ "$#" -eq 1 ]; then
    install -d -m 755 /etc/agentmail
    environment_stage=$(mktemp /etc/agentmail/.host.XXXXXXXX)
    socket_environment > "$environment_stage"
    mv -f -- "$environment_stage" "$environment"
    environment_stage=
fi
mv -f -- "$staged/agentmail-worker" "$staged/agentmail-notify" "$destination/"
if [ ! -e "$unit_dir/agentmail-mail.service" ]; then
    install -m 644 ./agentmail-mail.service "$unit_dir/"
fi
install -m 644 ./agentmail-notify.service "$unit_dir/"
systemctl daemon-reload
if [ "$stage_only" = true ]; then
    echo 'READY: agentmail units staged for the next VM boot.'
    exit 0
fi
systemctl restart agentmail-notify.service
systemctl --no-pager status agentmail-notify.service
