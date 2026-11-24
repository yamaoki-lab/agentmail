#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")"

if [ "$(id -un)" != agentmail ] || [ "$HOME" != /home/agentmail ]; then
    echo 'Run as the VM agentmail user.' >&2
    exit 1
fi

destination="$HOME/agentmail-ssh-lab"
test -x "$destination/agentmail-ssh"
exec 9>"$HOME/.ssh/agentmail-operation.lock"
if ! flock -x -w 10 9; then
    echo 'Timed out waiting for the agentmail operation lock.' >&2
    exit 1
fi
IFS= read -r XDG_RUNTIME_DIR < "$destination/runtime-dir"
test -d "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

podman_lab() {
    /usr/bin/podman --cgroup-manager=cgroupfs "$@" 9>&-
}

container=agentmail-mail-lab
gateway=$(mktemp "$destination/.agentmail-ssh.XXXXXXXX")
stage=
cleanup() {
    rm -f -- "$gateway"
    if [ -n "$stage" ]; then
        podman_lab exec "$container" rm -rf -- "$stage" >/dev/null || :
    fi
}
trap cleanup 0
install -m 700 ./agentmail-ssh "$gateway"
"$gateway" --help >/dev/null
candidate=$(podman_lab exec "$container" mktemp -d /opt/.agentmail-patch.XXXXXXXX)
case "$candidate" in
    /opt/.agentmail-patch.*) stage=$candidate ;;
    *) echo 'Unexpected mail CLI staging directory.' >&2; exit 1 ;;
esac
podman_lab cp ./agentmail "$container:$stage/agentmail"
podman_lab exec "$container" "$stage/agentmail" --identity beta inbox --unread >/dev/null
podman_lab exec "$container" mv -f -- "$stage/agentmail" /opt/agentmail-ssh-lab
mv -f -- "$gateway" "$destination/agentmail-ssh"
echo 'READY: SSH patch upload installed.'
