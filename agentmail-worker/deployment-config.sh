# Read literal key=value options. The deployment file is data and is never sourced.
read_deployment_config() {
    MAC_SSH_HOST= MAC_SSH_PORT= MAC_IPV4= MAC_SSH_KEY= MAC_KNOWN_HOSTS=
    CODEX_SSH_HOST= CODEX_SSH_PORT= CODEX_IPV4= CODEX_SSH_KEY= CODEX_KNOWN_HOSTS=
    CODEX_HOST_HOME=
    seen=' '
    line=0
    while IFS= read -r entry || [ -n "$entry" ]; do
        line=$((line + 1))
        case "$entry" in
            ''|'#'*) continue ;;
            *=*) name=${entry%%=*}; value=${entry#*=} ;;
            *) printf 'Invalid configuration line %s\n' "$line" >&2; return 1 ;;
        esac
        case "$seen" in
            *" $name "*) printf 'Duplicate key at line %s\n' "$line" >&2; return 1 ;;
        esac
        case "$name" in
            MAC_SSH_HOST) MAC_SSH_HOST=$value ;;
            MAC_SSH_PORT) MAC_SSH_PORT=$value ;;
            MAC_IPV4) MAC_IPV4=$value ;;
            MAC_SSH_KEY) MAC_SSH_KEY=$value ;;
            MAC_KNOWN_HOSTS) MAC_KNOWN_HOSTS=$value ;;
            CODEX_SSH_HOST) CODEX_SSH_HOST=$value ;;
            CODEX_SSH_PORT) CODEX_SSH_PORT=$value ;;
            CODEX_IPV4) CODEX_IPV4=$value ;;
            CODEX_SSH_KEY) CODEX_SSH_KEY=$value ;;
            CODEX_KNOWN_HOSTS) CODEX_KNOWN_HOSTS=$value ;;
            CODEX_HOST_HOME) CODEX_HOST_HOME=$value ;;
            *) printf 'Unknown configuration key at line %s\n' "$line" >&2; return 1 ;;
        esac
        seen="$seen$name "
    done < "$1"
}

absolute_path() {
    case "$1" in
        /*) ;;
        *) printf '%s must be an absolute path\n' "$2" >&2; return 1 ;;
    esac
}

select_client_config() {
    case "$1" in
        mac) host=$MAC_SSH_HOST; port=$MAC_SSH_PORT; ipv4=$MAC_IPV4
             key=$MAC_SSH_KEY; known_hosts=$MAC_KNOWN_HOSTS ;;
        development) host=$CODEX_SSH_HOST; port=$CODEX_SSH_PORT; ipv4=$CODEX_IPV4
                     key=$CODEX_SSH_KEY; known_hosts=$CODEX_KNOWN_HOSTS ;;
        *) echo 'Unknown Agentmail client platform' >&2; return 1 ;;
    esac
    case "$host" in
        ''|-*|*[!a-zA-Z0-9._:%-]*) echo 'Invalid SSH host in deployment configuration' >&2; return 1 ;;
    esac
    case "$port" in
        ''|0*|??????*|*[!0-9]*) echo 'Invalid SSH port in deployment configuration' >&2; return 1 ;;
    esac
    if [ "$port" -gt 65535 ]; then
        echo 'Invalid SSH port in deployment configuration' >&2
        return 1
    fi
    case "$ipv4" in
        yes) ipv4=--ipv4 ;;
        no) ipv4= ;;
        *) echo 'Set the IPv4 option to yes or no' >&2; return 1 ;;
    esac
    absolute_path "$key" 'SSH key path' && absolute_path "$known_hosts" 'known_hosts path'
}

shell_quote() {
    escaped=$(printf '%s\n' "$1" | sed "s/'/'\\\\''/g")
    printf "'%s'" "$escaped"
}

double_quote() {
    escaped=$(printf '%s\n' "$1" | sed 's/[\\"]/\\&/g')
    printf '"%s"' "$escaped"
}

socket_environment() {
    absolute_path "$CODEX_HOST_HOME" CODEX_HOST_HOME || return 1
    printf 'AGENTMAIL_SOCKET_DIRECTORY=%s\n' "$(double_quote "${CODEX_HOST_HOME%/}/.agentmail-notify")"
}
