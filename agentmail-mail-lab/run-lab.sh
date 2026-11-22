#!/bin/sh
set -eu
cd "$(dirname "$0")"

if [ "$(id -u)" -eq 0 ] || [ "$#" -ne 1 ]; then
    echo "Run as the dedicated non-root VM user: sh run-lab.sh /path/to/Linux/target/release" >&2
    exit 1
fi

podman_lab() {
    podman --cgroup-manager=cgroupfs "$@"
}

if podman_lab container exists agentmail-mail-lab; then
    echo "agentmail-mail-lab already exists. Run its smoke test with podman exec." >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
cp Containerfile entrypoint.sh postfix-main.cf dovecot.conf smoke.py "$stage/"
install -m 755 "$1/agentmail" "$1/agentmail-mailboxes" "$stage/"
podman_lab build --isolation=chroot --build-arg AGENTMAIL_TEST_TOOLS=python3 \
    -t localhost/agentmail-mail-lab:phase1 "$stage"
podman_lab volume create --ignore agentmail-mail-lab-maildir
podman_lab run --cgroups=disabled --detach --name agentmail-mail-lab \
    --network none \
    --volume agentmail-mail-lab-maildir:/data \
    localhost/agentmail-mail-lab:phase1
if ! podman_lab exec agentmail-mail-lab python3 /opt/mail-lab/smoke.py; then
    podman_lab logs --tail 80 agentmail-mail-lab >&2
    exit 1
fi
