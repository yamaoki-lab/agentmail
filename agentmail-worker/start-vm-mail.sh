#!/bin/sh
set -eu
umask 077
if [ "$(id -un)" != agentmail ]; then
    echo "Run as the VM's agentmail user." >&2
    exit 1
fi

cd "$HOME/agentmail-ssh-lab"
container=agentmail-mail-lab
podman_lab() {
    /usr/bin/podman --cgroup-manager=cgroupfs "$@" 9>&-
}

exec 9>"$HOME/.ssh/agentmail-operation.lock"
if ! flock -x -w 10 9; then
    echo 'Timed out waiting for the agentmail operation lock.' >&2
    exit 1
fi
test -x ./agentmail-ssh
volume=$(podman_lab inspect --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' "$container")
if [ "$volume" != agentmail-mail-lab-maildir ]; then
    echo "Expected the existing agentmail Maildir volume at /data." >&2
    exit 1
fi
if [ "$(podman_lab inspect --format '{{.State.Running}}' "$container")" != true ]; then
    podman_lab start "$container"
fi

runroot=$(podman_lab info --format '{{.Store.RunRoot}}')
runtime_dir=${runroot%/containers}
cid=$(podman_lab inspect --format '{{.Id}}' "$container")
if [ "$runtime_dir" = "$runroot" ] || [ ! -f "$runtime_dir/crun/$cid/status" ]; then
    echo "Could not confirm the live crun state under $runtime_dir." >&2
    exit 1
fi
export XDG_RUNTIME_DIR="$runtime_dir"
podman_lab exec "$container" /opt/mail-lab/agentmail-mailboxes ready
for identity in alpha beta; do
    podman_lab exec "$container" /opt/agentmail-ssh-lab --identity "$identity" inbox --unread >/dev/null
done

runtime_tmp=$(mktemp ./runtime-dir.XXXXXXXX)
trap 'rm -f -- "$runtime_tmp"' EXIT
printf '%s\n' "$runtime_dir" > "$runtime_tmp"
mv -f -- "$runtime_tmp" runtime-dir
printf 'READY: SMTP and both IMAP inboxes checked on %s\n' "$volume"
