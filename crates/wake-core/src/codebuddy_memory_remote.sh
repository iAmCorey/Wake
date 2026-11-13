# Read-only discovery with sh and rsync; no filesystem writes, external
# parsers, find, readlink, or hash utilities. NUL records are framed separately
# from login-shell banners. Never interpolate source paths into commands.
set +x
LC_ALL=C; CDPATH=; export LC_ALL CDPATH
cd "$HOME" || exit 2

wake_memory_fail() {
    printf '%s\n' 'Cannot safely read CodeBuddy memory source' >&2
    exit 2
}

wake_memory_path() {
    case "$1" in ''|/*|*[[:cntrl:]]*|*/../*|../*|*/..|*/./*|./*) wake_memory_fail;; esac
    wake_memory_parent=$1
    while :; do
        [ ! -L "$wake_memory_parent" ] || wake_memory_fail
        if [ -e "$wake_memory_parent" ]; then
            if [ "$wake_memory_parent" != "$1" ]; then
                [ -d "$wake_memory_parent" ] && [ -x "$wake_memory_parent" ] || wake_memory_fail
            fi
        fi
        case "$wake_memory_parent" in */*) wake_memory_parent=${wake_memory_parent%/*};; *) break;; esac
    done
}

wake_memory_walk() (
    wake_memory_dir=$1; wake_memory_recursive=$2; wake_memory_mdc=$3; wake_memory_depth=$4
    [ "$wake_memory_depth" -le 64 ] || wake_memory_fail
    wake_memory_path "$wake_memory_dir"
    [ -e "$wake_memory_dir" ] || exit 0
    [ -d "$wake_memory_dir" ] && [ -r "$wake_memory_dir" ] && [ -x "$wake_memory_dir" ] || wake_memory_fail
    printf 'D\000%s\000' "$wake_memory_dir"
    for wake_memory_child in "$wake_memory_dir"/*; do
        [ -e "$wake_memory_child" ] || [ -L "$wake_memory_child" ] || continue
        wake_memory_path "$wake_memory_child"
        if [ -d "$wake_memory_child" ]; then
            if [ "$wake_memory_recursive" = 1 ]; then
                wake_memory_walk "$wake_memory_child" 1 "$wake_memory_mdc" "$((wake_memory_depth+1))" || exit 2
            fi
        else
            case "$wake_memory_child" in
                *.md) :;;
                *.mdc) [ "$wake_memory_mdc" = 1 ] || continue;;
                *) continue;;
            esac
            [ -f "$wake_memory_child" ] && [ -r "$wake_memory_child" ] || wake_memory_fail
            printf 'F\000%s\000' "$wake_memory_child"
        fi
    done
)

wake_memory_projects() (
    wake_memory_root=$1; wake_memory_old=$2
    wake_memory_path "$wake_memory_root"
    [ -e "$wake_memory_root" ] || exit 0
    [ -d "$wake_memory_root" ] && [ -r "$wake_memory_root" ] && [ -x "$wake_memory_root" ] || wake_memory_fail
    printf 'D\000%s\000' "$wake_memory_root"
    for wake_memory_project in "$wake_memory_root"/*; do
        [ -e "$wake_memory_project" ] || [ -L "$wake_memory_project" ] || continue
        wake_memory_path "$wake_memory_project"
        [ -d "$wake_memory_project" ] || continue
        if [ "$wake_memory_old" = 1 ]; then wake_memory_dir=$wake_memory_project
        else wake_memory_dir=$wake_memory_project/memory; fi
        wake_memory_walk "$wake_memory_dir" 1 0 0 || exit 2
    done
)

wake_memory_config() (
    wake_memory_path "$2"
    printf 'C\000%s\000' "$1"
    if [ -e "$2" ]; then
        [ -f "$2" ] && [ -r "$2" ] || wake_memory_fail
        wake_memory_config_path=$2
        # --list-only sends no content and writes nothing. Both GNU rsync and
        # openrsync expose mode and byte size as their first two fields. GNU
        # rsync may use thousands separators; remove those with builtins.
        wake_memory_listing=$(rsync --list-only -- "$2" 2>/dev/null) || wake_memory_fail
        set -f; IFS=' '
        set -- $wake_memory_listing
        [ $# -ge 2 ] || wake_memory_fail
        case "$1" in -*) :;; *) wake_memory_fail;; esac
        wake_memory_size=$2
        while :; do
            case "$wake_memory_size" in
                *,*) wake_memory_size=${wake_memory_size%%,*}${wake_memory_size#*,};;
                *) break;;
            esac
        done
        case "$wake_memory_size" in ''|*[!0-9]*) wake_memory_fail;; esac
        [ "${#wake_memory_size}" -le 7 ] && [ "$wake_memory_size" -le 4194304 ] || wake_memory_fail
        wake_json "$wake_memory_size" < "$wake_memory_config_path" || exit 2
    else printf '{}'; fi
    printf '\000'
)

printf 'WAKE_MEMORY_V1\000'
case "$1" in
    inventory)
        wake_memory_projects .codebuddy/projects 0 || exit 2
        wake_memory_projects .codebuddy/memories 1 || exit 2
        wake_memory_walk .local/share/CodeBuddyExtension/Data/Public/.memories 0 1 0 || exit 2
        wake_memory_walk 'Library/Application Support/CodeBuddyExtension/Data/Public/.memories' 0 1 0 || exit 2
        # An unrelated host must not depend on CodeBuddy settings being valid.
        if [ -d .codebuddy/projects ] || [ -d .codebuddy/memories ]; then
            wake_memory_config cli .codebuddy/settings.json || exit 2
            wake_memory_config project .codebuddy/ide-memory-settings.json || exit 2
        fi
        if [ -d .local/share/CodeBuddyExtension/Data/Public/.memories ]; then
            wake_memory_config global_linux .local/share/CodeBuddyExtension/Data/Public/.memories/.memory-global-config.json || exit 2
        fi
        if [ -d 'Library/Application Support/CodeBuddyExtension/Data/Public/.memories' ]; then
            wake_memory_config global_mac 'Library/Application Support/CodeBuddyExtension/Data/Public/.memories/.memory-global-config.json' || exit 2
        fi;;
    resolve)
        wake_memory_index=0
        while IFS= read -r wake_memory_cwd; do
            wake_memory_index=$((wake_memory_index+1))
            case "$wake_memory_cwd" in /*) :;; *) wake_memory_fail;; esac
            case "$wake_memory_cwd" in *[[:cntrl:]]*) wake_memory_fail;; esac
            wake_memory_physical=$wake_memory_cwd
            if [ -d "$wake_memory_cwd" ]; then
                wake_memory_physical=$(cd -P "$wake_memory_cwd" 2>/dev/null && pwd -P) || wake_memory_fail
            elif [ -e "$wake_memory_cwd" ] || [ -L "$wake_memory_cwd" ]; then wake_memory_fail
            fi
            case "$wake_memory_physical" in *[[:cntrl:]]*) wake_memory_fail;; esac
            printf 'P\000%s\000%s\000' "$wake_memory_index" "$wake_memory_physical"
        done;;
    *) wake_memory_fail;;
esac
printf 'E\000'
