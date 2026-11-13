# Streaming JSON lexer using POSIX sh builtins. It emits a sanitized JSON
# document: only the four memory setting names, booleans, null and structure
# survive. Other strings become empty strings and numbers become zero. Rust
# validates the grammar and extracts switches; credentials never leave SSH.
# Split strings with IFS rather than large shell glob substitutions (quadratic
# on some shells). Each input line is consumed independently.
wake_json_fail() {
    printf '%s\n' 'Invalid or oversized CodeBuddy memory settings' >&2
    exit 2
}

wake_json_text() {
    case "$1" in *[[:cntrl:]]*) wake_json_fail;; esac
    [ "$wake_json_surrogate" = 0 ] || [ -z "$1" ] || wake_json_fail
    # Validate raw UTF-8 without sending arbitrary string values to Wake. Skip
    # entire ASCII runs; only multibyte sequences need per-byte processing.
    wake_json_utf=$1
    while :; do
        case "$wake_json_utf" in *$wake_json_nonascii*) :;; *) break;; esac
        wake_json_ascii=${wake_json_utf%%$wake_json_nonascii*}
        wake_json_utf=${wake_json_utf#"$wake_json_ascii"}
        wake_json_tail=${wake_json_utf#?}
        wake_json_byte=$(printf '%d' "'${wake_json_utf%"$wake_json_tail"}")
        if [ "$wake_json_byte" -lt 0 ]; then wake_json_byte=$((wake_json_byte+256)); fi
        wake_json_utf=$wake_json_tail
        wake_json_low=128; wake_json_high=191
        if [ "$wake_json_byte" -ge 194 ] && [ "$wake_json_byte" -le 223 ]; then
            wake_json_more=1
        elif [ "$wake_json_byte" -ge 224 ] && [ "$wake_json_byte" -le 239 ]; then
            wake_json_more=2
            if [ "$wake_json_byte" = 224 ]; then wake_json_low=160; fi
            if [ "$wake_json_byte" = 237 ]; then wake_json_high=159; fi
        elif [ "$wake_json_byte" -ge 240 ] && [ "$wake_json_byte" -le 244 ]; then
            wake_json_more=3
            if [ "$wake_json_byte" = 240 ]; then wake_json_low=144; fi
            if [ "$wake_json_byte" = 244 ]; then wake_json_high=143; fi
        else wake_json_fail; fi
        while [ "$wake_json_more" -gt 0 ]; do
            [ -n "$wake_json_utf" ] || wake_json_fail
            wake_json_tail=${wake_json_utf#?}
            wake_json_byte=$(printf '%d' "'${wake_json_utf%"$wake_json_tail"}")
            if [ "$wake_json_byte" -lt 0 ]; then wake_json_byte=$((wake_json_byte+256)); fi
            wake_json_utf=$wake_json_tail
            [ "$wake_json_byte" -ge "$wake_json_low" ] && [ "$wake_json_byte" -le "$wake_json_high" ] || wake_json_fail
            wake_json_more=$((wake_json_more-1)); wake_json_low=128; wake_json_high=191
        done
    done
    if [ "${#wake_json_name}" -le 32 ]; then
        if [ "${#1}" -le 32 ]; then wake_json_name=$wake_json_name$1
        else wake_json_name=XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX; fi
    fi
}

wake_json_string_part() {
    # Unquoted expansion is intentional and globbing is disabled by the caller.
    wake_json_split=$1'x'
    IFS='\'; set -- $wake_json_split; IFS=$wake_json_ifs
    while [ $# -gt 0 ]; do
        wake_json_part=$1; shift
        if [ $# -eq 0 ]; then wake_json_part=${wake_json_part%x}; fi
        if [ "$wake_json_escape" = 1 ] && [ -n "$wake_json_part" ]; then
            wake_json_rest=${wake_json_part#?}
            wake_json_char=${wake_json_part%"$wake_json_rest"}
            wake_json_part=$wake_json_rest
            [ "$wake_json_surrogate" = 0 ] || [ "$wake_json_char" = u ] || wake_json_fail
            case "$wake_json_char" in
                '"'|'\'|/) wake_json_text "$wake_json_char";;
                b|f|n|r|t) wake_json_text '?';;
                u)
                    [ "${#wake_json_part}" -ge 4 ] || wake_json_fail
                    wake_json_rest=${wake_json_part#????}
                    wake_json_hex=${wake_json_part%"$wake_json_rest"}
                    case "$wake_json_hex" in *[!0-9a-fA-F]*) wake_json_fail;; esac
                    wake_json_part=$wake_json_rest
                    wake_json_code=$((0x$wake_json_hex))
                    if [ "$wake_json_surrogate" = 1 ]; then
                        [ "$wake_json_code" -ge 56320 ] && [ "$wake_json_code" -le 57343 ] || wake_json_fail
                        wake_json_surrogate=0; wake_json_text '?'
                    elif [ "$wake_json_code" -ge 55296 ] && [ "$wake_json_code" -le 56319 ]; then
                        wake_json_surrogate=1
                    elif [ "$wake_json_code" -ge 56320 ] && [ "$wake_json_code" -le 57343 ]; then
                        wake_json_fail
                    elif [ "$wake_json_code" -ge 32 ] && [ "$wake_json_code" -le 126 ]; then
                        wake_json_oct=$(printf '%03o' "$wake_json_code")
                        wake_json_text "$(printf '%b' "\0$wake_json_oct")"
                    else wake_json_text '?'; fi;;
                *) wake_json_fail;;
            esac
            wake_json_escape=0
        fi
        wake_json_text "$wake_json_part"
        if [ $# -gt 0 ]; then
            if [ "$wake_json_escape" = 1 ]; then
                wake_json_text '\'; wake_json_escape=0
            else wake_json_escape=1; fi
        fi
    done
}

wake_json_digits() {
    [ -n "$1" ] || wake_json_fail
    case "$1" in *[!0-9]*) wake_json_fail;; esac
}

wake_json_scalar() {
    [ -n "$wake_json_atom" ] || return 0
    case "$wake_json_atom" in
        true|false|null) printf '%s ' "$wake_json_atom";;
        *)
            wake_json_num=${wake_json_atom#-}
            case "$wake_json_num" in *[!0-9.eE+-]*|'') wake_json_fail;; esac
            case "$wake_json_num" in *e|*E) wake_json_fail;; esac
            IFS=eE; set -- $wake_json_num; IFS=$wake_json_ifs
            [ $# -le 2 ] || wake_json_fail
            case "$wake_json_num" in
                *[eE]*)
                    [ $# -eq 2 ] || wake_json_fail
                    wake_json_exp=$2
                    case "$wake_json_exp" in +*|-*) wake_json_exp=${wake_json_exp#?};; esac
                    wake_json_digits "$wake_json_exp";;
            esac
            wake_json_mantissa=$1
            case "$wake_json_mantissa" in *.) wake_json_fail;; esac
            IFS=.; set -- $wake_json_mantissa; IFS=$wake_json_ifs
            [ $# -le 2 ] && [ $# -ge 1 ] || wake_json_fail
            case "$wake_json_mantissa" in
                *.*) [ $# -eq 2 ] || wake_json_fail; wake_json_digits "$2";;
            esac
            wake_json_digits "$1"
            case "$1" in 0[0-9]*) wake_json_fail;; esac
            printf '0 ';;
    esac
    wake_json_atom=
}

wake_json_outside() {
    wake_json_out=$1
    while [ -n "$wake_json_out" ]; do
        wake_json_rest=${wake_json_out#?}
        wake_json_char=${wake_json_out%"$wake_json_rest"}
        wake_json_out=$wake_json_rest
        case "$wake_json_char" in
            '{'|'}'|'['|']'|','|:) wake_json_scalar; printf '%s' "$wake_json_char";;
            ' '|"$wake_json_tab"|"$wake_json_cr") wake_json_scalar;;
            *)
                wake_json_atom=$wake_json_atom$wake_json_char
                [ "${#wake_json_atom}" -le 256 ] || wake_json_fail;;
        esac
    done
}

wake_json_line() {
    wake_json_split=$1'x'
    IFS='"'; set -- $wake_json_split; IFS=$wake_json_ifs
    while [ $# -gt 0 ]; do
        wake_json_piece=$1; shift
        if [ $# -eq 0 ]; then wake_json_piece=${wake_json_piece%x}; fi
        if [ "$wake_json_in_string" = 1 ]; then wake_json_string_part "$wake_json_piece"
        else wake_json_outside "$wake_json_piece"; fi
        if [ $# -gt 0 ]; then
            if [ "$wake_json_in_string" = 0 ]; then
                wake_json_scalar
                wake_json_in_string=1; wake_json_name=; wake_json_escape=0
            elif [ "$wake_json_escape" = 1 ]; then
                wake_json_text '"'; wake_json_escape=0
            else
                [ "$wake_json_surrogate" = 0 ] || wake_json_fail
                case "$wake_json_name" in
                    memory|autoMemoryEnabled|projectEnabled|enabled)
                        printf '"%s"' "$wake_json_name";;
                    *) printf '""';;
                esac
                wake_json_in_string=0
            fi
        fi
    done
    [ "$wake_json_in_string" = 0 ] || wake_json_fail
    wake_json_scalar
}

wake_json() (
    set -f
    wake_json_ifs=' '
    wake_json_nonascii='[! -~]'
    wake_json_tab=$(printf '\t'); wake_json_cr=$(printf '\r')
    wake_json_in_string=0; wake_json_atom=; wake_json_bytes=0; wake_json_surrogate=0
    while :; do
        wake_json_input=; wake_json_newline=0
        if IFS= read -r wake_json_input; then wake_json_newline=1
        elif [ -z "$wake_json_input" ]; then break; fi
        wake_json_bytes=$((wake_json_bytes + ${#wake_json_input} + wake_json_newline))
        [ "$wake_json_bytes" -le 4194304 ] || wake_json_fail
        wake_json_line "$wake_json_input"
        [ "$wake_json_newline" = 1 ] || break
    done
    [ "$wake_json_bytes" -gt 0 ] || wake_json_fail
    # POSIX shells cannot retain NUL bytes. A size mismatch detects discarded
    # bytes as well as concurrent rewrites; the caller gets size from rsync.
    [ $# -eq 0 ] || [ "$wake_json_bytes" = "$1" ] || wake_json_fail
)
