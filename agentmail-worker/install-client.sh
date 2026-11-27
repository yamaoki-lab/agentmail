#!/bin/sh
set -eu
umask 077
source_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd)
. "$source_dir/deployment-config.sh"

case "$#:${1:-}" in
    2:mac|2:development|3:mac|3:development) ;;
    *) echo 'Usage: install-client.sh mac|development CONFIG [BINARY_DIRECTORY]' >&2; exit 2 ;;
esac
read_deployment_config "$2"
select_client_config "$1"
binary_directory=${3:-"$source_dir/target/release"}
binary_names="agentmail-ssh agentmail-hook"
if [ "$1" = mac ]; then
    binary_names="$binary_names agentmail-install-claude-hooks"
fi
for name in $binary_names; do
    if [ ! -f "$binary_directory/$name" ] || [ ! -x "$binary_directory/$name" ]; then
        printf 'Missing executable: %s/%s\n' "$binary_directory" "$name" >&2
        exit 1
    fi
done
for path in "$key" "$known_hosts"; do
    if [ ! -f "$path" ] || [ ! -r "$path" ]; then
        printf 'Cannot read required file: %s\n' "$path" >&2
        exit 1
    fi
done

command_dir="$HOME/.local/bin"
client_dir="$HOME/.local/lib/agentmail"
command_path="$command_dir/agentmail"
marker='# Installed by agentmail install-client.sh'
if [ -L "$command_path" ] ||
    { [ -e "$command_path" ] && { [ ! -f "$command_path" ] || ! grep -Fxq "$marker" "$command_path"; }; }; then
    printf 'Existing command at %s. Move it before installing.\n' "$command_path" >&2
    exit 1
fi

mkdir -p "$command_dir" "$client_dir"
stage=$(mktemp -d "$client_dir/.install.XXXXXXXX")
wrapper=
trap 'rm -rf -- "$stage"; if [ -n "$wrapper" ]; then rm -f -- "$wrapper"; fi' EXIT
for name in $binary_names; do
    install -m 700 "$binary_directory/$name" "$stage/$name"
    if [ "$name" != agentmail-hook ] && ! "$stage/$name" --help >/dev/null; then
        printf 'Cannot execute %s on this machine\n' "$name" >&2
        exit 1
    fi
done
install -m 600 "$known_hosts" "$stage/known-hosts"
wrapper=$(mktemp "$command_dir/.agentmail.XXXXXXXX")
{
    printf '%s\n' '#!/bin/sh' "$marker"
    printf 'exec "$HOME/.local/lib/agentmail/agentmail-ssh" --host %s --port %s %s --key %s' \
        "$(shell_quote "$host")" "$port" "$ipv4" "$(shell_quote "$key")"
    printf '%s\n' ' --known-hosts "$HOME/.local/lib/agentmail/known-hosts" "$@"'
} > "$wrapper"
chmod 700 "$wrapper"

mv -f -- "$stage/agentmail-ssh" "$stage/agentmail-hook" "$stage/known-hosts" "$client_dir/"
if [ "$1" = mac ]; then
    # Stage in the destination directory so the live command is replaced atomically.
    hook_installer=$(mktemp "$command_dir/.agentmail-install-claude-hooks.XXXXXXXX")
    trap 'rm -rf -- "$stage"; rm -f -- "$wrapper" "$hook_installer"' EXIT
    install -m 700 "$stage/agentmail-install-claude-hooks" "$hook_installer"
    mv -f -- "$hook_installer" "$command_dir/agentmail-install-claude-hooks"
fi
mv -f -- "$wrapper" "$command_path"
printf 'Installed: %s\n' "$command_path"
