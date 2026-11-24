#!/bin/sh
set -eu

if [ "$(id -un)" != agentmail ]; then
    echo 'Run as the VM agentmail user.' >&2
    exit 1
fi
container=agentmail-mail-lab
IFS= read -r XDG_RUNTIME_DIR < "$HOME/agentmail-ssh-lab/runtime-dir"
export XDG_RUNTIME_DIR
podman_lab() {
    /usr/bin/podman --cgroup-manager=cgroupfs "$@" 9>&-
}

case "$#:${1:-}:${2:-}" in
    1:install:)
        cd "$(dirname "$0")"
        exec 9>"$HOME/.ssh/agentmail-operation.lock"
        if ! flock -x -w 10 9; then
            echo 'Timed out waiting for the agentmail operation lock.' >&2
            exit 1
        fi
        printf '%s\n' 'Installing mailbox management into the running container...'
        stage=$(podman_lab exec "$container" mktemp -d /opt/mail-lab/.agentmail-install.XXXXXXXX)
        trap 'podman_lab exec "$container" rm -rf -- "$stage" >/dev/null' EXIT
        podman_lab cp ./agentmail-mailboxes "$container:$stage/agentmail-mailboxes"
        podman_lab cp ./entrypoint.sh "$container:$stage/entrypoint.sh"
        podman_lab exec "$container" chmod 755 "$stage/agentmail-mailboxes"
        podman_lab exec "$container" "$stage/agentmail-mailboxes" ready
        podman_lab exec "$container" bash -n "$stage/entrypoint.sh"
        podman_lab exec "$container" mv -- "$stage/agentmail-mailboxes" /opt/mail-lab/agentmail-mailboxes
        podman_lab exec "$container" mv -- "$stage/entrypoint.sh" /opt/mail-lab/entrypoint.sh
        printf '%s\n' 'Mailbox management installed.'
        ;;
    2:identity:list)
        podman_lab exec "$container" /opt/mail-lab/agentmail-mailboxes list
        ;;
    3:identity:create)
        printf '%s\n' 'Updating mailbox configuration and checking SMTP/IMAP...'
        podman_lab exec "$container" /opt/mail-lab/agentmail-mailboxes create "$3"
        ;;
    *)
        echo 'Usage: manage-vm-mailboxes.sh install | identity list | identity create NAME' >&2
        exit 2
        ;;
esac
