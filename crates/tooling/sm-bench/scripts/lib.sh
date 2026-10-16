# Shared guards for the sm-bench scripts. Source this file from bash.
# Error text is part of the guard contract.

# A path component named loadtest. A prefix or suffix does not count.
has_loadtest() {
    case "/$1/" in
        */loadtest/*) return 0 ;;
        *) return 1 ;;
    esac
}

# Canonicalize the deepest existing ancestor, then apply the missing tail.
# `..` in that tail is applied, so loadtest/../../outside does not keep the component.
resolve_path() {
    local path=$1
    local -a missing=()
    local base parent
    while [ ! -e "$path" ] && [ ! -L "$path" ]; do
        base=$(basename -- "$path")
        parent=$(dirname -- "$path")
        if [ "$parent" = "$path" ]; then
            echo "cannot resolve $1" >&2
            return 1
        fi
        missing+=("$base")
        if [ -z "$parent" ]; then
            path=.
            break
        fi
        path=$parent
    done
    local canon part
    local i
    canon=$(readlink -f "$path") || return 1
    for ((i = ${#missing[@]} - 1; i >= 0; i--)); do
        part=${missing[$i]}
        if [ "$part" = "." ]; then
            continue
        fi
        if [ "$part" = ".." ]; then
            canon=$(dirname -- "$canon")
            continue
        fi
        canon=$canon/$part
    done
    printf '%s\n' "$canon"
}

require_loadtest() {
    local label=$1
    local path=$2
    if ! has_loadtest "$path"; then
        echo "$label $path has no loadtest path component; refusing" >&2
        exit 1
    fi
    local resolved
    resolved=$(resolve_path "$path") || exit 1
    if ! has_loadtest "$resolved"; then
        echo "$label resolves to $resolved, which has no loadtest path component; refusing" >&2
        exit 1
    fi
}

# The queue write and the sampler must use the disk that holds this path.
require_mount_device() {
    local canon=$1
    local dev=$2
    local mnt_src mnt
    if command -v findmnt >/dev/null 2>&1; then
        mnt_src=$(findmnt -n -o SOURCE -T "$canon" || true)
        mnt=$(findmnt -n -o TARGET -T "$canon" || true)
    else
        mnt_src=$(df -P "$canon" | awk 'NR==2 { print $1 }')
        mnt=$(df -P "$canon" | awk 'NR==2 { print $6 }')
    fi
    if [ -z "${mnt_src:-}" ] || [ -z "${mnt:-}" ]; then
        echo "no mount for $canon" >&2
        exit 1
    fi
    # findmnt appends a subvolume in brackets. The lab disk does not.
    mnt_src=${mnt_src%%[*}
    mnt_src=$(readlink -f "$mnt_src") || {
        echo "cannot resolve mount source for $canon" >&2
        exit 1
    }
    local mnt_name parent
    mnt_name=${mnt_src##*/}
    if [ -e "/sys/class/block/$mnt_name/partition" ]; then
        parent=$(basename "$(readlink -f "/sys/class/block/$mnt_name/..")")
    else
        parent=$mnt_name
    fi
    if [ "$parent" != "$dev" ]; then
        echo "refusing: $canon is on $mnt_name, not $dev" >&2
        exit 1
    fi
}

mount_target() {
    local path=$1
    local target
    if command -v findmnt >/dev/null 2>&1; then
        target=$(findmnt -n -o TARGET -T "$path" || true)
    else
        target=$(df -P "$path" | awk 'NR==2 { print $6 }')
    fi
    if [ -z "${target:-}" ]; then
        echo "no mount for $path" >&2
        exit 1
    fi
    readlink -f "$target"
}

# A mapper name such as dm-0 contains a hyphen.
# Empty, slash, and "." / ".." stay refused.
require_device_name() {
    case "$1" in
        "" | . | .. | *[!A-Za-z0-9-]*)
            echo "device name must be alphanumeric or '-': $1" >&2
            exit 1
            ;;
    esac
}
