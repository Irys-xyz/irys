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

# Absolute path with . and .. collapsed. Symlinks stay in the path.
# A relative path starts at the physical working directory.
lexical_absolute() {
    local path=$1
    local out="/"
    local part rest
    if [ "${path:0:1}" != "/" ]; then
        path=$(pwd -P)/$path
    fi
    rest=${path#/}
    while [ -n "$rest" ]; do
        part=${rest%%/*}
        case "$rest" in
            */*) rest=${rest#*/} ;;
            *) rest= ;;
        esac
        case "$part" in
            "" | ".") ;;
            "..") out=$(dirname -- "$out") ;;
            *)
                if [ "$out" = "/" ]; then
                    out="/$part"
                else
                    out="$out/$part"
                fi
                ;;
        esac
    done
    printf '%s\n' "$out"
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
    # mkdir and the bench follow a symlink ancestor. The parent the operator
    # named must be the parent after resolution.
    local named_parent resolved_parent
    named_parent=$(dirname -- "$(lexical_absolute "$path")")
    resolved_parent=$(dirname -- "$resolved")
    if [ "$named_parent" != "$resolved_parent" ]; then
        echo "refusing $label $path: it resolves under $resolved_parent, not $named_parent" >&2
        exit 1
    fi
}

# Refuse when pm2 lists the app "Irys Node" as online, or when its jlist
# cannot be read. A missing pm2 binary is allowed.
refuse_if_irys_node_online() {
    if ! command -v pm2 >/dev/null 2>&1; then
        return 0
    fi
    local jlist pm_status
    jlist=$(pm2 jlist 2>/dev/null || true)
    pm_status=0
    printf '%s' "$jlist" | python3 -c '
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    sys.exit(2)
try:
    apps = json.loads(raw)
except json.JSONDecodeError:
    sys.exit(2)
if not isinstance(apps, list):
    sys.exit(2)
online = False
for app in apps:
    if not isinstance(app, dict):
        continue
    env = app.get("pm2_env") or {}
    if app.get("name") == "Irys Node" and env.get("status") == "online":
        online = True
        break
sys.exit(0 if online else 1)
' || pm_status=$?
    if [ "$pm_status" -eq 0 ]; then
        echo "Irys Node is online; refusing" >&2
        exit 1
    fi
    if [ "$pm_status" -ne 1 ]; then
        echo "pm2 jlist was not readable; refusing" >&2
        exit 1
    fi
}

# Signed bash arithmetic wraps at 2^63. 8589934592 GiB is that boundary,
# so a larger count is not a floor. Accept 0 through 8589934591.
require_free_bytes() {
    local path=$1
    local gib_text=$2
    case "$gib_text" in
        '' | *[!0-9]*)
            echo "min free GiB must be an integer from 0 through 8589934591: $gib_text" >&2
            exit 1
            ;;
    esac
    if [ "${#gib_text}" -gt 10 ]; then
        echo "min free GiB must be an integer from 0 through 8589934591: $gib_text" >&2
        exit 1
    fi
    local gib avail_bytes min_free_bytes
    gib=$((10#$gib_text))
    if [ "$gib" -gt 8589934591 ]; then
        echo "min free GiB must be an integer from 0 through 8589934591: $gib_text" >&2
        exit 1
    fi
    avail_bytes=$(df -B1 --output=avail "$path" | tail -n 1)
    avail_bytes=${avail_bytes//[[:space:]]/}
    min_free_bytes=$((gib * 1024 * 1024 * 1024))
    if [ "$avail_bytes" -lt "$min_free_bytes" ]; then
        echo "$path has ${avail_bytes} bytes free, below ${gib_text} GiB; refusing" >&2
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
