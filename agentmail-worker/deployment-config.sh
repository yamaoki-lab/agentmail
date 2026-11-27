# Read literal key=value options. The deployment file is data and is never sourced.
read_deployment_config() {
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

double_quote() {
    escaped=$(printf '%s\n' "$1" | sed 's/[\\"]/\\&/g')
    printf '"%s"' "$escaped"
}

socket_environment() {
    absolute_path "$CODEX_HOST_HOME" CODEX_HOST_HOME || return 1
    printf 'AGENTMAIL_SOCKET_DIRECTORY=%s\n' "$(double_quote "${CODEX_HOST_HOME%/}/.agentmail-notify")"
}
