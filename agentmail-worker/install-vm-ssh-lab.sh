#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "$(id -un)" != agentmail ]; then
    echo "Run as the VM's agentmail user." >&2
    exit 1
fi

umask 077
ssh_dir="$HOME/.ssh"
authorized="$ssh_dir/authorized_keys"
gateway="$PWD/agentmail-ssh"
case "$gateway" in
    /home/agentmail/agentmail-ssh-lab/agentmail-ssh) ;;
    *) echo "Unexpected gateway location." >&2; exit 1 ;;
esac

case $# in
    0)
        public_key=development.pub
        identity=beta
        key_label=agentmail-dev-lab
        ;;
    2)
        if [ "$1" != --register-mac ]; then
            echo 'Usage: install-vm-ssh-lab.sh [--register-mac /absolute/path/to/key.pub]' >&2
            exit 2
        fi
        case "$2" in
            /*) public_key=$2 ;;
            *) echo 'Use an absolute path to the Mac public key.' >&2; exit 2 ;;
        esac
        identity=alpha
        key_label=agentmail-mac-lab
        ;;
    *) echo 'Usage: install-vm-ssh-lab.sh [--register-mac /absolute/path/to/key.pub]' >&2; exit 2 ;;
esac

if ! awk 'END { exit (NR != 1) }' "$public_key"; then
    echo 'Expected exactly one public key.' >&2
    exit 1
fi
ssh-keygen -lf "$public_key" >&2
read -r algorithm key ignored < "$public_key"
case "$algorithm" in
    ssh-ed25519) ;;
    *) echo "Expected an Ed25519 test key." >&2; exit 1 ;;
esac
case "$key" in
    ''|*[!A-Za-z0-9+/=]*) echo "Invalid key data." >&2; exit 1 ;;
esac
line="restrict,command=\"$gateway --serve $identity\" $algorithm $key $key_label"

mkdir -p "$ssh_dir"
exec 9> "$ssh_dir/agentmail-operation.lock"
if ! flock -x -w 10 9; then
    echo 'Timed out waiting for the agentmail operation lock.' >&2
    exit 1
fi
if [ -e "$authorized" ]; then
    # A matching blob under any other options is a conflict, even if the desired line also exists.
    if ! awk -v blob="$key" -v desired="$line" '
        $1 !~ /^#/ && $0 != desired {
            for (i = 2; i <= NF; i++) if ($(i - 1) == "ssh-ed25519" && $i == blob) exit 1
        }
    ' "$authorized"; then
        echo "The test key already exists with other options. No key was added." >&2
        exit 1
    fi
fi

podman_lab() {
    /usr/bin/podman --cgroup-manager=cgroupfs "$@" 9>&-
}
if [ "$identity" = beta ]; then
    runroot=$(podman_lab info --format '{{.Store.RunRoot}}')
    runtime_dir=${runroot%/containers}
    cid=$(podman_lab inspect --format '{{.Id}}' agentmail-mail-lab)
    if [ "$runtime_dir" = "$runroot" ] || [ ! -f "$runtime_dir/crun/$cid/status" ]; then
        echo "Could not confirm the live crun state under $runtime_dir." >&2
        exit 1
    fi
    podman_lab cp ./agentmail agentmail-mail-lab:/opt/agentmail-ssh-lab
else
    if [ ! -x "$gateway" ] || [ ! -r runtime-dir ]; then
        echo 'Install the existing VM SSH gateway before registering the Mac key.' >&2
        exit 1
    fi
    IFS= read -r runtime_dir < runtime-dir
fi
XDG_RUNTIME_DIR="$runtime_dir" podman_lab exec agentmail-mail-lab /opt/agentmail-ssh-lab --help >/dev/null
if [ "$identity" = beta ]; then
    printf '%s\n' "$runtime_dir" > runtime-dir
fi
printf 'Podman runtime verified: %s\n' "$runtime_dir"
if ! grep -Fxq -- "$line" "$authorized" 2>/dev/null; then
    printf '\n%s\n' "$line" >> "$authorized"
fi
printf 'READY: test key installed for %s\n' "$identity"
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
