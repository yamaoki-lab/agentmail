#!/bin/sh
set -eu
umask 077
if [ "$#" -gt 1 ]; then
    echo 'Usage: install-vm-sessions.sh [linux-binary-directory]' >&2
    exit 1
fi
binary_directory=${1:-$(dirname "$0")}
binary_directory=$(cd "$binary_directory" && pwd)
for binary in agentmail-ssh agentmail-admin; do
    if [ ! -x "$binary_directory/$binary" ] ||
        [ "$(LC_ALL=C od -An -N4 -tx1 "$binary_directory/$binary" | tr -d '[:space:]')" != 7f454c46 ]; then
        echo "Missing Linux binary $binary in $binary_directory. Build with cargo build --release --locked --bin agentmail-ssh --bin agentmail-admin and pass the output directory." >&2
        exit 1
    fi
done

if [ "$(id -un)" != agentmail ] || [ "$HOME" != /home/agentmail ]; then
    echo 'Run as the VM agentmail user.' >&2
    exit 1
fi

destination="$HOME/agentmail-ssh-lab"
test -x "$destination/agentmail-ssh"
test -r "$destination/runtime-dir"
exec 9>"$HOME/.ssh/agentmail-operation.lock"
if ! flock -x -w 10 9; then
    echo 'Timed out waiting for the agentmail operation lock.' >&2
    exit 1
fi

stage=$(mktemp -d "$destination/.sessions.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
install -m 700 "$binary_directory/agentmail-ssh" "$stage/agentmail-ssh"
install -m 700 "$binary_directory/agentmail-admin" "$stage/agentmail-admin"
"$stage/agentmail-ssh" --help >/dev/null
"$stage/agentmail-admin" --help >/dev/null

mv -f -- "$stage/agentmail-ssh" "$destination/agentmail-ssh"
mv -f -- "$stage/agentmail-admin" "$destination/agentmail-admin"
echo 'READY: VM session registration installed.'
