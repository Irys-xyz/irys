#!/bin/bash
# Regression checks for the sm-bench shell guards.
# Throwaway directories under /tmp, plus one on the worktree disk so a
# success case is not tied to source name "none". Does not write real
# devices or sysfs.
set -u
set -o pipefail

here=$(cd "$(dirname "$0")" && pwd)
fail=0

ok() { printf 'ok %s\n' "$1"; }
bad() {
    printf 'FAIL %s: %s\n' "$1" "$2" >&2
    fail=1
}

tmpdir() { mktemp -d /tmp/sm-bench-guards.XXXXXX; }

# Source basename require_mount_device compares when the fake /sys has no
# partition node. Call it on a path that exists.
mount_dev_for() {
    local path=$1
    local mnt_src
    if command -v findmnt >/dev/null 2>&1; then
        mnt_src=$(findmnt -n -o SOURCE -T "$path" || true)
    else
        mnt_src=$(df -P "$path" | awk 'NR==2 { print $1 }')
    fi
    mnt_src=${mnt_src%%[*}
    mnt_src=$(readlink -f "$mnt_src") || return 1
    printf '%s\n' "${mnt_src##*/}"
}

mount_of() {
    local path=$1
    local target
    if command -v findmnt >/dev/null 2>&1; then
        target=$(findmnt -n -o TARGET -T "$path" || true)
    else
        target=$(df -P "$path" | awk 'NR==2 { print $6 }')
    fi
    printf '%s\n' "$target"
}

# A temp directory on the worktree disk, which is not the /tmp tmpfs.
other_tmpdir() {
    local base
    base=$(cd "$here/../../../.." && pwd)/.tmp
    mkdir -p "$base"
    mktemp -d "$base/sm-bench-guards.XXXXXX"
}

stub_bench() {
    local root=$1
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
}

# safe/loadtest and a queue for the disk that holds it.
loadtest_disk() {
    local root=$1
    local -n _dev=$2
    local -n _mnt=$3
    mkdir -p "$root/safe/loadtest"
    _dev=$(mount_dev_for "$root/safe/loadtest")
    _mnt=$(mount_of "$root/safe/loadtest")
    queue_files "$root/sys/block/$_dev/queue"
}

# A pid that has already exited.
finished_pid() {
    local pid
    sleep 30 &
    pid=$!
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    printf '%s\n' "$pid"
}

# bpftrace that records its pid and exits on INT or TERM.
# GNU sleep ignores SIGINT, so this stub cannot be sleep.
hold_bpftrace() {
    local root=$1
    mkdir -p "$root/bin"
    cat >"$root/bin/bpftrace" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bpf.pid"
exec python3 -c 'import signal,sys,time
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
time.sleep(120)'
EOF
    chmod +x "$root/bin/bpftrace"
}

# $1 ok-name, $2 charset|later, $3 needle, $4 1 to require that the bench did not run.
# Uses the caller's err file and root.
name_guard() {
    local ok_name=$1 mode=$2 needle=$3 check_bench=$4
    shift 4
    if "$@" >"$err" 2>&1; then
        bad "$ok_name" "exited 0: $(cat "$err")"
        return
    fi
    if [ "$mode" = charset ]; then
        if ! grep -q "$needle" "$err"; then
            bad "$ok_name" "did not reject: $(cat "$err")"
            return
        fi
    else
        if grep -q "alphanumeric" "$err"; then
            bad "$ok_name" "rejected the name: $(cat "$err")"
            return
        fi
        if ! grep -q "$needle" "$err"; then
            bad "$ok_name" "did not reach the later check: $(cat "$err")"
            return
        fi
    fi
    if [ "$check_bench" = 1 ] && [ -e "$root/bench-ran" ]; then
        bad "$ok_name" "bench ran"
        return
    fi
    ok "$ok_name"
}

test_fill_refuses_file_outside_loadtest() {
    local name=fill-outside root file log err
    root=$(tmpdir)
    file=$root/live.dat
    log=$root/fill.log
    err=$root/err
    printf 'LIVE-NODE-BYTES' >"$file"
    if "$here/fill.sh" --file "$file" --log "$log" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "no loadtest path component" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$file"; then
        bad "$name" "file bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_symlink_that_leaves_loadtest() {
    local name=fill-symlink root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/outside/live.dat"
    ln -s "$root/outside/live.dat" "$root/loadtest/link.dat"
    if "$here/fill.sh" --file "$root/loadtest/link.dat" --log "$root/fill.log" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "no loadtest path component" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$root/outside/live.dat"; then
        bad "$name" "symlink target bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# The target directory still contains loadtest, but it is not the named parent.
# The shared guard refuses that pair before dd.
test_fill_refuses_a_symlink_into_another_loadtest_directory() {
    local name=fill-symlink-other-loadtest root err victim
    root=$(tmpdir)
    err=$root/err
    victim=$root/other/loadtest/victim
    mkdir -p "$root/bench/loadtest" "$root/other/loadtest"
    python3 -c 'import sys; sys.stdout.buffer.write(b"A"*4096)' >"$victim"
    ln -s "$victim" "$root/bench/loadtest/write-region.dat"
    if "$here/fill.sh" --file "$root/bench/loadtest/write-region.dat" \
        --log "$root/bench/loadtest/fill.log" --segment-bytes 4096 --block 4096 \
        >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "resolves under" "$err"; then
        bad "$name" "stderr did not refuse the parent: $(cat "$err")"
    elif ! cmp -s <(python3 -c 'import sys; sys.stdout.buffer.write(b"A"*4096)') "$victim"; then
        bad "$name" "victim bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_a_symlink_pid_and_log() {
    local name=fill-pid-symlink root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    printf 'SECRET-BYTES' >"$root/outside/secret"
    printf 'LOG-ORIGINAL' >"$root/outside/log"
    ln -s "$root/outside/secret" "$root/loadtest/pid"
    ln -s "$root/outside/log" "$root/loadtest/fill.log"
    if "$here/fill.sh" --file "$root/loadtest/data" --log "$root/loadtest/fill.log" \
        --pid-file "$root/loadtest/pid" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! cmp -s <(printf 'SECRET-BYTES') "$root/outside/secret"; then
        bad "$name" "pid symlink target changed: $(od -An -tx1 "$root/outside/secret")"
    elif ! cmp -s <(printf 'LOG-ORIGINAL') "$root/outside/log"; then
        bad "$name" "log symlink target changed"
    elif [ ! -L "$root/loadtest/pid" ] || [ ! -L "$root/loadtest/fill.log" ]; then
        bad "$name" "symlink was removed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_a_log_through_a_symlink_parent() {
    local name=fill-log-parent root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    printf 'DATA_OK' >"$root/outside/keep"
    ln -s "$root/outside" "$root/loadtest/link"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/loadtest/data" --log "$root/loadtest/link/notes.txt" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif [ -e "$root/outside/notes.txt" ]; then
        bad "$name" "log was created outside loadtest: $(cat "$root/outside/notes.txt")"
    elif ! cmp -s <(printf 'DATA_OK') "$root/outside/keep"; then
        bad "$name" "outside bytes changed"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$root/loadtest/data"; then
        bad "$name" "data file changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_to_append_a_log_through_a_symlink_parent() {
    local name=fill-log-parent-append root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    printf 'LOG-ORIGINAL\n' >"$root/outside/notes.txt"
    ln -s "$root/outside" "$root/loadtest/link"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/loadtest/data" --log "$root/loadtest/link/notes.txt" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! cmp -s <(printf 'LOG-ORIGINAL\n') "$root/outside/notes.txt"; then
        bad "$name" "log was appended outside loadtest: $(cat "$root/outside/notes.txt")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_a_log_under_a_missing_dir_through_a_symlink() {
    local name=fill-log-ancestor root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    ln -s "$root/outside" "$root/loadtest/link"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/loadtest/data" \
        --log "$root/loadtest/link/sub/notes.txt" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif [ -e "$root/outside/sub" ] || [ -e "$root/outside/notes.txt" ]; then
        bad "$name" "mkdir followed the symlink ancestor"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# bench/loadtest/link -> other/loadtest. The log path is not itself a
# symlink. mkdir and append must not follow it into the other tree.
test_fill_refuses_a_log_through_a_symlink_into_another_loadtest() {
    local name=fill-log-other-loadtest root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/bench/loadtest" "$root/other/loadtest"
    printf 'LIVE-NODE-BYTES' >"$root/bench/loadtest/data"
    printf 'KEEP-INTERVALS\n' >"$root/other/loadtest/intervals.json"
    ln -s "$root/other/loadtest" "$root/bench/loadtest/link"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/bench/loadtest/data" \
        --log "$root/bench/loadtest/link/intervals.json" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0: $(cat "$err")"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! cmp -s <(printf 'KEEP-INTERVALS\n') "$root/other/loadtest/intervals.json"; then
        bad "$name" "victim intervals changed: $(cat "$root/other/loadtest/intervals.json")"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$root/bench/loadtest/data"; then
        bad "$name" "data file changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# /alias/loadtest -> /victim/loadtest. The file is a real file under another
# loadtest directory. The log must not follow the loadtest symlink.
test_fill_refuses_a_log_when_the_loadtest_directory_is_a_symlink() {
    local name=fill-log-loadtest-symlink root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/bench/loadtest" "$root/victim/loadtest" "$root/alias"
    printf 'SAFE-DATA' >"$root/bench/loadtest/data"
    printf 'KEEP-INTERVALS\n' >"$root/victim/loadtest/intervals.json"
    ln -s "$root/victim/loadtest" "$root/alias/loadtest"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/bench/loadtest/data" \
        --log "$root/alias/loadtest/intervals.json" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0: $(cat "$err")"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! cmp -s <(printf 'KEEP-INTERVALS\n') "$root/victim/loadtest/intervals.json"; then
        bad "$name" "victim intervals changed: $(cat "$root/victim/loadtest/intervals.json")"
    elif ! cmp -s <(printf 'SAFE-DATA') "$root/bench/loadtest/data"; then
        bad "$name" "data file changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# /link/via -> /victim, and the log is /link/via/loadtest/intervals.json.
# An ancestor symlink must not hide the other tree.
test_fill_refuses_a_log_when_a_loadtest_ancestor_is_a_symlink() {
    local name=fill-log-ancestor-symlink root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/bench/loadtest" "$root/victim/loadtest" "$root/link"
    printf 'SAFE-DATA' >"$root/bench/loadtest/data"
    printf 'KEEP-INTERVALS\n' >"$root/victim/loadtest/intervals.json"
    ln -s "$root/victim" "$root/link/via"
    touch "$root/STOP"
    if "$here/fill.sh" --file "$root/bench/loadtest/data" \
        --log "$root/link/via/loadtest/intervals.json" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0: $(cat "$err")"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! cmp -s <(printf 'KEEP-INTERVALS\n') "$root/victim/loadtest/intervals.json"; then
        bad "$name" "victim intervals changed: $(cat "$root/victim/loadtest/intervals.json")"
    elif ! cmp -s <(printf 'SAFE-DATA') "$root/bench/loadtest/data"; then
        bad "$name" "data file changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_appends_a_log_when_the_symlink_stays_under_loadtest() {
    local name=fill-log-parent-inside root
    root=$(tmpdir)
    mkdir -p "$root/loadtest/real"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    ln -s "$root/loadtest/real" "$root/loadtest/link"
    touch "$root/STOP"
    if ! "$here/fill.sh" --file "$root/loadtest/data" --log "$root/loadtest/link/notes.txt" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$root/err" 2>&1; then
        bad "$name" "fill.sh refused a log that stays under loadtest: $(cat "$root/err")"
    elif [ ! -f "$root/loadtest/real/notes.txt" ]; then
        bad "$name" "log was not written under loadtest"
    elif ! grep -q '^# start ' "$root/loadtest/real/notes.txt"; then
        bad "$name" "log has no start line: $(cat "$root/loadtest/real/notes.txt")"
    elif ! grep -q '^# stopped at 0 ' "$root/loadtest/real/notes.txt"; then
        bad "$name" "log has no stop line: $(cat "$root/loadtest/real/notes.txt")"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$root/loadtest/data"; then
        bad "$name" "data file changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_creates_a_missing_log_dir_under_loadtest() {
    local name=fill-log-missing-dir root
    root=$(tmpdir)
    mkdir -p "$root/loadtest"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    touch "$root/STOP"
    if ! "$here/fill.sh" --file "$root/loadtest/data" \
        --log "$root/loadtest/fresh/notes.txt" \
        --stop-file "$root/STOP" --segment-bytes 16 >"$root/err" 2>&1; then
        bad "$name" "fill.sh refused a new directory under loadtest: $(cat "$root/err")"
    elif [ ! -f "$root/loadtest/fresh/notes.txt" ]; then
        bad "$name" "log was not created"
    elif ! grep -q '^# start ' "$root/loadtest/fresh/notes.txt"; then
        bad "$name" "log has no start line"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_refuses_a_pid_file_through_a_symlink_parent() {
    local name=fill-pid-parent root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/outside"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    printf 'SECRET-BYTES' >"$root/outside/secret"
    ln -s "$root/outside" "$root/loadtest/out"
    if "$here/fill.sh" --file "$root/loadtest/data" --log "$root/loadtest/fill.log" \
        --pid-file "$root/loadtest/out/pid" --stop-file "$root/STOP" --segment-bytes 16 \
        >"$err" 2>&1; then
        bad "$name" "fill.sh exited 0"
    elif ! grep -q "loadtest" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif [ -e "$root/outside/pid" ]; then
        bad "$name" "pid file was created outside loadtest"
    elif ! cmp -s <(printf 'SECRET-BYTES') "$root/outside/secret"; then
        bad "$name" "outside bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_term_stops_dd() {
    local name=fill-term-dd root err fill_pid dd_pid tries
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/loadtest" "$root/bin"
    printf 'LIVE-NODE-BYTES' >"$root/loadtest/data"
    cat >"$root/bin/dd" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/dd.pid"
sleep 60
EOF
    chmod +x "$root/bin/dd"
    PATH="$root/bin:$PATH" "$here/fill.sh" --file "$root/loadtest/data" \
        --log "$root/loadtest/fill.log" --pid-file "$root/loadtest/fill.pid" \
        --segment-bytes 16 >"$err" 2>&1 &
    fill_pid=$!
    if ! wait_file "$root/dd.pid" || ! wait_file "$root/loadtest/fill.pid"; then
        bad "$name" "fill did not start: $(cat "$err" 2>/dev/null)"
    else
        dd_pid=$(cat "$root/dd.pid")
        kill -TERM "$(cat "$root/loadtest/fill.pid")" 2>/dev/null || true
        tries=0
        while alive "$dd_pid" || alive "$fill_pid"; do
            tries=$((tries + 1))
            if [ "$tries" -ge 20 ]; then
                break
            fi
            sleep 0.1
        done
        if alive "$dd_pid"; then
            bad "$name" "dd $dd_pid still running after TERM"
        elif alive "$fill_pid"; then
            bad "$name" "fill.sh still running after TERM"
        elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$root/loadtest/data"; then
            bad "$name" "data file changed"
        else
            ok "$name"
        fi
    fi
    kill "$fill_pid" 2>/dev/null || true
    if [ -s "$root/dd.pid" ]; then
        kill "$(cat "$root/dd.pid")" 2>/dev/null || true
    fi
    wait "$fill_pid" 2>/dev/null || true
    rm -rf "$root"
}

# Same directory: the parent check allows the alias. The fill must zero the
# target, and the log size must be the target length, not the symlink length.
test_fill_zeroes_through_a_same_directory_symlink() {
    local name=fill-symlink-same-dir root log target
    root=$(tmpdir)
    log=$root/loadtest/fill.log
    target=$root/loadtest/region.dat
    mkdir -p "$root/loadtest"
    python3 -c 'import sys; sys.stdout.buffer.write(b"C"*4096)' >"$target"
    ln -s region.dat "$root/loadtest/alias.dat"
    if ! "$here/fill.sh" --file "$root/loadtest/alias.dat" --log "$log" \
        --segment-bytes 4096 --block 4096 >"$root/err" 2>&1; then
        bad "$name" "fill.sh refused a same-directory symlink: $(cat "$root/err")"
    elif ! grep -q 'size=4096' "$log"; then
        bad "$name" "log size is not the target length: $(head -n 1 "$log" 2>/dev/null)"
    elif ! cmp -s <(python3 -c 'import sys; sys.stdout.buffer.write(b"\0"*4096)') "$target"; then
        bad "$name" "target was not fully zeroed"
    elif [ ! -L "$root/loadtest/alias.dat" ]; then
        bad "$name" "alias is no longer a symlink"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A regular file has no symlink length. The whole file is still zeroed.
test_fill_zeroes_a_regular_file() {
    local name=fill-regular-file root log target
    root=$(tmpdir)
    log=$root/loadtest/fill.log
    target=$root/loadtest/region.dat
    mkdir -p "$root/loadtest"
    python3 -c 'import sys; sys.stdout.buffer.write(b"C"*4096)' >"$target"
    if ! "$here/fill.sh" --file "$target" --log "$log" \
        --segment-bytes 4096 --block 4096 >"$root/err" 2>&1; then
        bad "$name" "fill.sh refused a regular file: $(cat "$root/err")"
    elif ! grep -q 'size=4096' "$log"; then
        bad "$name" "log size: $(head -n 1 "$log" 2>/dev/null)"
    elif ! cmp -s <(python3 -c 'import sys; sys.stdout.buffer.write(b"\0"*4096)') "$target"; then
        bad "$name" "file was not fully zeroed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_fill_allows_a_loadtest_file_and_stops_before_write() {
    local name=fill-allow root file
    root=$(tmpdir)
    file=$root/loadtest/live.dat
    mkdir -p "$root/loadtest"
    printf 'LIVE-NODE-BYTES' >"$file"
    touch "$root/STOP"
    if ! "$here/fill.sh" --file "$file" --log "$root/loadtest/fill.log" --stop-file "$root/STOP" --segment-bytes 16 >"$root/err" 2>&1; then
        bad "$name" "fill.sh refused a loadtest path: $(cat "$root/err")"
    elif ! cmp -s <(printf 'LIVE-NODE-BYTES') "$file"; then
        bad "$name" "file bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

seed_module() {
    local dir=$1 item
    mkdir -p "$dir"
    for item in db rocks intervals.json packing_params.toml bench-prepop.json chunks.dat; do
        printf 'keep-%s\n' "$item" >"$dir/$item"
    done
}

state_in_place() {
    local dir=$1 item
    for item in db rocks intervals.json packing_params.toml bench-prepop.json chunks.dat; do
        [ -f "$dir/$item" ] || return 1
    done
    [ ! -e "$dir/state-prior" ]
}

with_fake_sys() {
    local sys=$1
    shift
    unshare --mount --user --map-root-user bash -c '
        mount --bind "$1" /sys || exit 97
        shift
        exec "$@"
    ' bash "$sys" "$@"
}

# Optional bind of $2 onto $3 inside the same mount namespace. An empty $2 skips it.
with_binds() {
    local sys=$1 src=$2 dst=$3
    shift 3
    unshare --mount --user --map-root-user bash -c '
        mount --bind "$1" /sys || exit 97
        if [ -n "$2" ]; then
            mount --bind "$2" "$3" || exit 97
        fi
        shift 3
        exec "$@"
    ' bash "$sys" "$src" "$dst" "$@"
}

# Bind $2 onto $3 and $4 onto $5 inside the same mount namespace as /sys.
with_two_binds() {
    local sys=$1 src1=$2 dst1=$3 src2=$4 dst2=$5
    shift 5
    unshare --mount --user --map-root-user bash -c '
        mount --bind "$1" /sys || exit 97
        mount --bind "$2" "$3" || exit 97
        mount --bind "$4" "$5" || exit 97
        shift 5
        exec "$@"
    ' bash "$sys" "$src1" "$dst1" "$src2" "$dst2" "$@"
}

fake_sys() {
    local sys=$1
    mkdir -p "$sys/block/loop0/queue"
    printf '111\n' >"$sys/block/loop0/queue/max_sectors_kb"
}

test_ladder_does_not_stash_without_loadtest() {
    local name=ladder-no-loadtest root module sys err
    root=$(tmpdir)
    module=$root/var/lib/irys-module
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    fake_sys "$sys"
    stub_bench "$root"
    if with_fake_sys "$sys" "$here/ladder.sh" \
        --device loop0 --mount /tmp --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0"
    elif ! grep -q "no loadtest path component" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was moved"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_does_not_write_queue_when_mount_disk_differs() {
    local name=ladder-mount-disk root module sys err
    root=$(tmpdir)
    module=$root/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    fake_sys "$sys"
    stub_bench "$root"
    if with_fake_sys "$sys" "$here/ladder.sh" \
        --device loop0 --mount /tmp --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --max-sectors-kb 64 --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0"
    elif ! grep -q "not loop0" "$err"; then
        bad "$name" "stderr did not refuse the disk: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was moved"
    elif ! grep -q '^111$' "$sys/block/loop0/queue/max_sectors_kb"; then
        bad "$name" "queue value changed: $(cat "$sys/block/loop0/queue/max_sectors_kb")"
    elif grep -q 'max_sectors_kb=111' "$err"; then
        bad "$name" "queue was written and restored: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

queue_files() {
    local queue=$1
    mkdir -p "$queue"
    printf '111\n' >"$queue/max_sectors_kb"
    printf '128\n' >"$queue/read_ahead_kb"
    printf '64\n' >"$queue/nr_requests"
    printf '[mq-deadline] none\n' >"$queue/scheduler"
}

test_run_refuses_a_missing_path_that_escapes_loadtest() {
    local name=run-dotdot root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest" "$root/sys/block/none/queue"
    queue_files "$root/sys/block/none/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device none --mount /tmp \
        --loadtest "$root/safe/loadtest" \
        --recall-file "$root/safe/loadtest/../../escaped-recall.dat" \
        --write-file "$root/safe/loadtest/write-region.dat" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0: $(cat "$err")"
    elif ! grep -q "no loadtest path component" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_rejects_a_dotdot_name() {
    local name=run-name-dotdot root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest" "$root/out" "$root/sys/block/none/queue"
    queue_files "$root/sys/block/none/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name .. --device none --mount /tmp \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0"
    elif ! grep -q "single path component" "$err"; then
        bad "$name" "stderr did not refuse the name: $(cat "$err")"
    elif [ -e "$root/sysfs.txt" ] || [ -e "$root/run.pid" ]; then
        bad "$name" "run files were written above --out-root"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_rejects_a_dot_name() {
    local name=run-name-dot root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest" "$root/out" "$root/sys/block/none/queue"
    queue_files "$root/sys/block/none/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name . --device none --mount /tmp \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0"
    elif ! grep -q "single path component" "$err"; then
        bad "$name" "stderr did not refuse the name: $(cat "$err")"
    elif [ -e "$root/out/sysfs.txt" ]; then
        bad "$name" "run files were written into --out-root itself"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_rejects_a_dotdot_name() {
    local name=ladder-name-dotdot root module sys err
    root=$(tmpdir)
    module=$root/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$sys/block/none/queue" "$root/out"
    queue_files "$sys/block/none/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device none --mount /tmp --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --run '.. only' \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0"
    elif ! grep -q "single path component" "$err"; then
        bad "$name" "stderr did not refuse the name: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was moved"
    elif [ -e "$root/sysfs.txt" ]; then
        bad "$name" "run files were written above --out-root"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_a_device_that_is_not_the_mount_disk() {
    local name=run-mount-disk root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest" "$root/sys/block/loop0/queue"
    queue_files "$root/sys/block/loop0/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device loop0 --mount /tmp \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0: $(cat "$err")"
    elif ! grep -q "not loop0" "$err"; then
        bad "$name" "stderr did not refuse the disk: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif [ -e "$root/out/t/sysfs.txt" ]; then
        bad "$name" "sysfs snapshot was written for the other disk"
    elif ! grep -q '^111$' "$root/sys/block/loop0/queue/max_sectors_kb"; then
        bad "$name" "queue value changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_a_missing_path_when_the_disk_is_not_named_none() {
    local name=run-allow-other-disk root err dev
    root=$(other_tmpdir)
    err=$root/err
    dev=$(mount_dev_for "$root")
    if [ "$dev" = "none" ]; then
        bad "$name" "$root is source none; this case does not exercise another disk"
        rm -rf "$root"
        return
    fi
    mkdir -p "$root/safe/loadtest" "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    stub_bench "$root"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$(mount_of "$root")" \
        --loadtest "$root/safe/loadtest/newdir" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a path under loadtest on $dev: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_a_missing_path_under_loadtest() {
    local name=run-allow-missing root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest/newdir" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a path under loadtest: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

cmdline_has() {
    local pid=$1 needle=$2
    tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q "$needle"
}

test_ladder_term_stops_the_run_and_restores_the_queue() {
    local name=ladder-term root module sys err outer dev mnt
    local bench_pid run_parent ladder_pid tries
    root=$(tmpdir)
    module=$root/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bench.pid"
exec sleep 60
EOF
    chmod +x "$root/bench"
    MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --max-sectors-kb 64 --run only-run \
        >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/bench.pid"; then
        bad "$name" "bench did not start: $(cat "$err" 2>/dev/null)"
    else
        bench_pid=$(cat "$root/bench.pid")
        run_parent=$(ps -o ppid= -p "$bench_pid" | tr -d '[:space:]')
        ladder_pid=$(ps -o ppid= -p "$run_parent" | tr -d '[:space:]')
        if ! cmdline_has "$ladder_pid" "ladder.sh"; then
            bad "$name" "parent $ladder_pid is not ladder.sh"
        else
            kill -TERM "$ladder_pid" 2>/dev/null || true
            tries=0
            while alive "$bench_pid" || alive "$ladder_pid"; do
                tries=$((tries + 1))
                if [ "$tries" -ge 20 ]; then
                    break
                fi
                sleep 0.1
            done
            if alive "$bench_pid"; then
                bad "$name" "bench $bench_pid still running after TERM"
            elif alive "$ladder_pid"; then
                bad "$name" "ladder $ladder_pid still running after TERM"
            elif ! grep -q '^111$' "$sys/block/$dev/queue/max_sectors_kb"; then
                bad "$name" "queue was not restored: $(cat "$sys/block/$dev/queue/max_sectors_kb" 2>/dev/null)"
            else
                ok "$name"
            fi
        fi
    fi
    kill "$outer" 2>/dev/null || true
    if [ -s "$root/bench.pid" ]; then
        kill "$(cat "$root/bench.pid")" 2>/dev/null || true
    fi
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_ladder_term_stops_the_tracer() {
    local name=ladder-trace-term root module sys err outer dev mnt
    local tracer_pid ladder_pid tries
    root=$(tmpdir)
    module=$root/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    printf '#!/bin/sh\nexit 0\n' >"$root/bench"
    chmod +x "$root/bench"
    cat >"$root/blk-run.sh" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/tracer.pid"
exec sleep 60
EOF
    chmod +x "$root/blk-run.sh"
    MIN_FREE_GIB=0 with_binds "$sys" "$root/blk-run.sh" "$here/blk-run.sh" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --max-sectors-kb 64 --trace --run only-run \
        >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/tracer.pid"; then
        bad "$name" "tracer did not start: $(cat "$err" 2>/dev/null)"
    else
        tracer_pid=$(cat "$root/tracer.pid")
        ladder_pid=$(ps -o ppid= -p "$tracer_pid" | tr -d '[:space:]')
        if ! cmdline_has "$ladder_pid" "ladder.sh"; then
            bad "$name" "parent $ladder_pid is not ladder.sh"
        else
            kill -TERM "$ladder_pid" 2>/dev/null || true
            tries=0
            while alive "$tracer_pid" || alive "$ladder_pid"; do
                tries=$((tries + 1))
                if [ "$tries" -ge 20 ]; then
                    break
                fi
                sleep 0.1
            done
            if alive "$tracer_pid"; then
                bad "$name" "tracer $tracer_pid still running after TERM"
            elif alive "$ladder_pid"; then
                bad "$name" "ladder $ladder_pid still running after TERM"
            elif ! grep -q '^111$' "$sys/block/$dev/queue/max_sectors_kb"; then
                bad "$name" "queue was not restored: $(cat "$sys/block/$dev/queue/max_sectors_kb" 2>/dev/null)"
            else
                ok "$name"
            fi
        fi
    fi
    kill "$outer" 2>/dev/null || true
    if [ -s "$root/tracer.pid" ]; then
        kill "$(cat "$root/tracer.pid")" 2>/dev/null || true
    fi
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_ladder_does_not_stash_when_recall_file_escapes() {
    local name=ladder-recall-dotdot root module sys err
    root=$(tmpdir)
    module=$root/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$sys/block/none/queue"
    stub_bench "$root"
    if with_fake_sys "$sys" "$here/ladder.sh" \
        --device none --mount /tmp --loadtest "$module" \
        --recall-file "$module/../../escaped-recall.dat" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0"
    elif ! grep -q "no loadtest path component" "$err"; then
        bad "$name" "stderr did not refuse: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was moved"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

wait_file() {
    local path=$1 tries=0
    while [ ! -s "$path" ]; do
        tries=$((tries + 1))
        if [ "$tries" -ge 50 ]; then
            return 1
        fi
        sleep 0.1
    done
}

alive() { kill -0 "$1" 2>/dev/null; }

test_run_records_the_bench_and_term_stops_it() {
    local name=run-term root err outer dev mnt
    local bench_pid run_pid
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    cat >"$root/bench" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bench.pid"
printf '%s\n' "\$PPID" > "$root/bench.ppid"
exec sleep 60
EOF
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/bench.pid" || ! wait_file "$root/out/t/run.pid"; then
        bad "$name" "bench did not start: $(cat "$err" 2>/dev/null)"
    else
        bench_pid=$(cat "$root/bench.pid")
        run_pid=$(cat "$root/out/t/run.pid")
        if [ "$run_pid" != "$bench_pid" ]; then
            bad "$name" "run.pid=$run_pid bench=$bench_pid shell=$(cat "$root/bench.ppid" 2>/dev/null)"
        else
            kill -TERM "$(cat "$root/bench.ppid")" 2>/dev/null || true
            local tries=0
            while alive "$bench_pid"; do
                tries=$((tries + 1))
                if [ "$tries" -ge 20 ]; then
                    break
                fi
                sleep 0.1
            done
            if alive "$bench_pid"; then
                bad "$name" "bench $bench_pid still running after TERM to run.sh"
            else
                ok "$name"
            fi
        fi
    fi
    kill "$outer" 2>/dev/null || true
    if [ -s "$root/bench.pid" ]; then
        kill "$(cat "$root/bench.pid")" 2>/dev/null || true
    fi
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_teardown_waits_for_the_group_before_restoring_the_queue() {
    local name=teardown-group root sys queue orig err
    local leader child
    root=$(tmpdir)
    sys=$root/sys
    queue=$sys/block/notadisk/queue
    orig=$root/run/sysfs.txt
    err=$root/err
    mkdir -p "$queue" "$root/run"
    printf '999\n' >"$queue/max_sectors_kb"
    printf '999\n' >"$queue/read_ahead_kb"
    printf '999\n' >"$queue/nr_requests"
    printf 'none\n' >"$queue/scheduler"
    cat >"$orig" <<'EOF'
max_sectors_kb=111
read_ahead_kb=128
nr_requests=64
scheduler=[mq-deadline]
EOF
    cat >"$root/leader.sh" <<EOF
#!/bin/bash
printf '%s' irys-sm-bench > /proc/\$\$/comm
printf '%s\n' "\$\$" > "$root/leader.pid"
trap 'sleep 0.3; cat "$queue/max_sectors_kb" > "$root/queue_seen"; exit 0' TERM
sleep 60 &
printf '%s\n' "\$!" > "$root/child.pid"
wait
EOF
    chmod +x "$root/leader.sh"
    setsid "$root/leader.sh" >/dev/null 2>&1 &
    if ! wait_file "$root/leader.pid" || ! wait_file "$root/child.pid"; then
        bad "$name" "leader did not start"
        rm -rf "$root"
        return
    fi
    leader=$(cat "$root/leader.pid")
    child=$(cat "$root/child.pid")
    printf '%s\n' "$leader" >"$root/run/run.pid"
    if ! with_fake_sys "$sys" "$here/teardown.sh" notadisk "$orig" >"$err" 2>&1; then
        bad "$name" "teardown failed: $(cat "$err")"
    elif ! wait_file "$root/queue_seen"; then
        bad "$name" "leader did not observe the queue: $(cat "$err")"
    elif ! grep -q '^999$' "$root/queue_seen"; then
        bad "$name" "queue changed while the leader was alive: $(cat "$root/queue_seen")"
    elif alive "$child"; then
        bad "$name" "child $child in the bench group is still running"
    elif ! grep -q '^111$' "$queue/max_sectors_kb"; then
        bad "$name" "queue was not restored"
    else
        ok "$name"
    fi
    kill "$leader" "$child" 2>/dev/null || true
    wait "$leader" "$child" 2>/dev/null || true
    rm -rf "$root"
}

test_teardown_signals_the_recorded_pid_only() {
    local name=teardown-pid root sys queue orig err
    local recorded decoy before
    if pgrep -x irys-sm-bench >/dev/null 2>&1; then
        bad "$name" "another irys-sm-bench is already running; not testing pkill"
        return
    fi
    root=$(tmpdir)
    sys=$root/sys
    queue=$sys/block/notadisk/queue
    orig=$root/run/sysfs.txt
    err=$root/err
    mkdir -p "$queue" "$root/run"
    printf '999\n' >"$queue/max_sectors_kb"
    printf '999\n' >"$queue/read_ahead_kb"
    printf '999\n' >"$queue/nr_requests"
    printf 'none\n' >"$queue/scheduler"
    cat >"$orig" <<'EOF'
max_sectors_kb=111
read_ahead_kb=128
nr_requests=64
scheduler=[mq-deadline]
EOF
    python3 -c 'import time; open("/proc/self/comm","w").write("irys-sm-bench"); time.sleep(60)' &
    recorded=$!
    printf '%s\n' "$recorded" >"$root/run/run.pid"
    printf '#!/bin/sh\nsleep 60\n' >"$root/irys-sm-bench"
    chmod +x "$root/irys-sm-bench"
    "$root/irys-sm-bench" &
    decoy=$!
    sleep 0.2
    before=$fail
    if ! with_fake_sys "$sys" "$here/teardown.sh" notadisk "$orig" >"$err" 2>&1; then
        bad "$name" "teardown failed: $(cat "$err")"
    fi
    if kill -0 "$recorded" 2>/dev/null; then
        bad "$name" "recorded pid $recorded is still running"
    fi
    if ! kill -0 "$decoy" 2>/dev/null; then
        bad "$name" "decoy irys-sm-bench $decoy was signaled"
    fi
    if ! grep -q '^111$' "$queue/max_sectors_kb"; then
        bad "$name" "max_sectors_kb=$(cat "$queue/max_sectors_kb" 2>/dev/null || echo missing)"
    fi
    if [ "$fail" -eq "$before" ]; then
        ok "$name"
    fi
    kill "$recorded" "$decoy" 2>/dev/null || true
    wait "$recorded" "$decoy" 2>/dev/null || true
    rm -rf "$root"
}

test_device_name_allows_a_hyphen_and_rejects_dot_and_slash() {
    local name=device-hyphen root err module sys queue
    root=$(tmpdir)
    err=$root/err
    module=$root/loadtest
    sys=$root/sys
    queue=$sys/block/dm-0/queue
    mkdir -p "$module" "$queue" "$sys/block/data-root/queue" "$root/out"
    queue_files "$queue"
    queue_files "$sys/block/data-root/queue"
    stub_bench "$root"

    local spec ok_name mode needle check_bench dev
    local -a run_rows=(
        'run|later|not dm-0|1|dm-0'
        'run-mapper|later|not data-root|0|data-root'
        'run-dotdot|charset|alphanumeric|1|..'
    )
    for spec in "${run_rows[@]}"; do
        IFS='|' read -r ok_name mode needle check_bench dev <<<"$spec"
        name_guard "$name-$ok_name" "$mode" "$needle" "$check_bench" \
            with_fake_sys "$sys" "$here/run.sh" \
            --name t --device "$dev" --mount /tmp \
            --loadtest "$module" \
            --out-root "$root/out" --bench "$root/bench" --min-free-gib 0
    done

    name_guard "$name-ladder" later "not dm-0" 0 \
        with_fake_sys "$sys" "$here/ladder.sh" \
        --device dm-0 --mount /tmp --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run

    printf '999\n' >"$queue/max_sectors_kb"
    cat >"$root/out/sysfs.txt" <<'EOF'
max_sectors_kb=111
read_ahead_kb=128
nr_requests=64
scheduler=[mq-deadline]
EOF
    # A finished pid is enough. The charset case is not a live bench.
    printf '%s\n' "$(finished_pid)" >"$root/out/run.pid"
    if ! with_fake_sys "$sys" "$here/teardown.sh" dm-0 "$root/out/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown.sh rejected dm-0: $(cat "$err")"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "teardown.sh said dm-0 is not a device name: $(cat "$err")"
    elif ! grep -q '^111$' "$queue/max_sectors_kb"; then
        bad "$name" "teardown.sh did not restore dm-0: $(cat "$queue/max_sectors_kb" 2>/dev/null)"
    else
        ok "$name-teardown"
    fi

    printf '999\n' >"$queue/max_sectors_kb"
    local -a teardown_rows=(
        'teardown-dot|.|1'
        'teardown-empty||0'
    )
    for spec in "${teardown_rows[@]}"; do
        IFS='|' read -r ok_name dev check_bench <<<"$spec"
        if with_fake_sys "$sys" "$here/teardown.sh" "$dev" "$root/out/sysfs.txt" >"$err" 2>&1; then
            bad "$name-$ok_name" "teardown.sh accepted ${dev:-an empty name}"
        elif ! grep -q "alphanumeric" "$err"; then
            bad "$name-$ok_name" "teardown.sh did not reject ${dev:-an empty name}: $(cat "$err")"
        elif [ "$check_bench" = 1 ] && ! grep -q '^999$' "$queue/max_sectors_kb"; then
            bad "$name-$ok_name" "teardown.sh wrote the queue for $dev"
        else
            ok "$name-$ok_name"
        fi
    done

    name_guard "$name-sample" later iostat 0 \
        "$here/sample.sh" dm-0 "$root/out"
    name_guard "$name-sample-slash" charset alphanumeric 0 \
        "$here/sample.sh" "sda/sdb" "$root/out"
    name_guard "$name-blk-run" later "not dm-0" 0 \
        "$here/blk-run.sh" dm-0 "$module" "$root/trace"
    name_guard "$name-blk-run-dotdot" charset alphanumeric 0 \
        "$here/blk-run.sh" .. "$module" "$root/trace"

    rm -rf "$root"
}

queue_restore_fixture() {
    local root=$1
    local sys=$root/sys
    local queue=$sys/block/notadisk/queue
    local orig=$root/run/sysfs.txt
    mkdir -p "$queue" "$root/run"
    printf '999\n' >"$queue/max_sectors_kb"
    printf '999\n' >"$queue/read_ahead_kb"
    printf '999\n' >"$queue/nr_requests"
    printf 'none\n' >"$queue/scheduler"
    cat >"$orig" <<'EOF'
max_sectors_kb=111
read_ahead_kb=128
nr_requests=64
scheduler=[mq-deadline]
EOF
}

# A bad run.pid must not restore the queue.
# Rows: ok-name, how to prepare the pid file, file text, stderr needle, exit-0 message.
test_teardown_does_not_restore_for_a_bad_run_pid() {
    local spec name mode payload needle exited root err
    local -a rows=(
        'teardown-bad-pid|write|not-a-pid|not a pid|teardown exited 0 for a non-numeric pid'
        'teardown-missing-pid|missing||missing|teardown exited 0 with no run.pid'
        'teardown-empty-pid|empty||not a pid|teardown exited 0 for an empty run.pid'
    )
    for spec in "${rows[@]}"; do
        IFS='|' read -r name mode payload needle exited <<<"$spec"
        root=$(tmpdir)
        err=$root/err
        queue_restore_fixture "$root"
        case "$mode" in
            write) printf '%s\n' "$payload" >"$root/run/run.pid" ;;
            empty) : >"$root/run/run.pid" ;;
            missing) ;;
        esac
        if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
            bad "$name" "$exited"
        elif ! grep -q "$needle" "$err"; then
            bad "$name" "stderr: $(cat "$err")"
        elif ! grep -q '^999$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
            bad "$name" "queue changed: $(cat "$root/sys/block/notadisk/queue/max_sectors_kb" 2>/dev/null)"
        else
            ok "$name"
        fi
        rm -rf "$root"
    done
}

test_teardown_does_not_signal_a_pid_whose_comm_is_not_the_bench() {
    local name=teardown-wrong-comm root err sleeper
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    sleep 60 &
    sleeper=$!
    printf '%s\n' "$sleeper" >"$root/run/run.pid"
    if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown exited 0 for comm $(cat "/proc/$sleeper/comm" 2>/dev/null || echo gone)"
    elif ! alive "$sleeper"; then
        bad "$name" "sleep $sleeper was signaled"
    elif ! grep -q '^999$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue changed while the other process was alive"
    else
        ok "$name"
    fi
    kill "$sleeper" 2>/dev/null || true
    wait "$sleeper" 2>/dev/null || true
    rm -rf "$root"
}

test_teardown_restores_when_the_numeric_pid_is_already_gone() {
    local name=teardown-dead-pid root err
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    printf '%s\n' "$(finished_pid)" >"$root/run/run.pid"
    if ! with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown refused a dead pid: $(cat "$err")"
    elif ! grep -q '^111$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue was not restored"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

install_rounding_df() {
    local root=$1
    mkdir -p "$root/bin"
    cat >"$root/bin/df" <<'EOF'
#!/bin/bash
if [ "$1" = "-B1G" ]; then
    printf 'Avail\n2\n'
    exit 0
fi
if [ "$1" = "-B1" ]; then
    # 1.6 GiB. df -B1G rounds this to 2, which is under a 2 GiB floor.
    printf 'Avail\n1717986918\n'
    exit 0
fi
exec /usr/bin/df "$@"
EOF
    chmod +x "$root/bin/df"
}

test_run_refuses_when_mount_is_not_the_loadtest_mount() {
    local name=run-mount-mismatch root err dev mnt
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    if [ "$mnt" = "$(mount_of /workspaces)" ]; then
        bad "$name" "loadtest mount $mnt is /workspaces; this case needs two mounts"
        rm -rf "$root"
        return
    fi
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount /workspaces \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh started when --mount was not the loadtest mount"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran on the wrong mount check"
    elif ! grep -q "not the loadtest mount" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_measures_free_space_on_the_loadtest_path() {
    local name=run-df-loadtest root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/bin"
    cat >"$root/bin/df" <<EOF
#!/bin/bash
printf '%s\\n' "\$*" >> "$root/df.args"
exec /usr/bin/df "\$@"
EOF
    chmod +x "$root/bin/df"
    stub_bench "$root"
    if ! PATH="$root/bin:$PATH" with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a matching mount: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif ! grep -F "$root/safe/loadtest" "$root/df.args" >/dev/null; then
        bad "$name" "df was not run on the loadtest path: $(cat "$root/df.args" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_the_floor_on_the_loadtest_mount() {
    local name=run-floor-same-mount root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 1 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused the loadtest mount: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_a_floor_that_df_would_round_up() {
    local name=run-floor-bytes root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    install_rounding_df "$root"
    stub_bench "$root"
    if PATH="$root/bin:$PATH" with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 2 \
        >"$err" 2>&1; then
        bad "$name" "run.sh accepted 1.6 GiB free against a 2 GiB floor"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran below the floor"
    elif ! grep -q "below 2" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_a_floor_under_the_byte_count() {
    local name=run-floor-allow root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    install_rounding_df "$root"
    stub_bench "$root"
    if ! PATH="$root/bin:$PATH" with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 1 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused 1.6 GiB free against a 1 GiB floor: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_removes_the_pid_after_the_bench_exits() {
    local name=run-pid-removed root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    cat >"$root/bench" <<'EOF'
#!/bin/bash
printf '%s' irys-sm-bench > /proc/$$/comm
exit 0
EOF
    chmod +x "$root/bench"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh failed: $(cat "$err")"
    elif [ -e "$root/out/t/run.pid" ]; then
        bad "$name" "run.pid still present after the bench exited: $(cat "$root/out/t/run.pid")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

block_dev() {
    local cand
    for cand in loop0 dm-0; do
        if [ -b "/dev/$cand" ]; then
            printf '%s\n' "$cand"
            return 0
        fi
    done
    return 1
}

prepare_blk_run_fakes() {
    local root=$1
    local dev=$2
    local sys=$root/sys
    mkdir -p "$root/bin" "$sys/class/block/$dev" "$sys/block/$dev" "$root/safe/loadtest" "$root/trace"
    printf '2048\n' >"$sys/class/block/$dev/size"
    printf '0 0 0 0 0 0 0 0 0 1 0\n' >"$sys/block/$dev/stat"
    cat >"$root/bin/findmnt" <<EOF
#!/bin/bash
case "\$*" in
    *SOURCE*) printf '%s\n' "/dev/$dev" ;;
    *TARGET*) printf '%s\n' "/tmp" ;;
esac
EOF
    chmod +x "$root/bin/findmnt"
}

test_blk_run_term_does_not_wait_out_sleep() {
    local name=blk-run-term-sleep root err dev outer blk tries
    root=$(tmpdir)
    err=$root/err
    dev=$(block_dev) || {
        bad "$name" "no loop0 or dm-0 block device"
        rm -rf "$root"
        return
    }
    prepare_blk_run_fakes "$root" "$dev"
    hold_bpftrace "$root"
    cat >"$root/bin/sleep" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >> "$root/sleep.log"
exec /bin/sleep "\$@"
EOF
    cat >"$root/extent-snap.sh" <<'EOF'
#!/bin/bash
exit 0
EOF
    chmod +x "$root/bin/sleep" "$root/extent-snap.sh"
    PATH="$root/bin:$PATH" with_binds "$root/sys" "$root/extent-snap.sh" "$here/extent-snap.sh" \
        "$here/blk-run.sh" "$dev" "$root/safe/loadtest" "$root/trace" >"$err" 2>&1 &
    outer=$!
    tries=0
    while [ ! -f "$root/sleep.log" ] || ! grep -qx 30 "$root/sleep.log"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 50 ] || ! alive "$outer"; then
            bad "$name" "did not reach sleep 30: $(cat "$err" 2>/dev/null) sleep=$(cat "$root/sleep.log" 2>/dev/null)"
            kill "$outer" 2>/dev/null || true
            wait "$outer" 2>/dev/null || true
            rm -rf "$root"
            return
        fi
        sleep 0.1
    done
    blk=$(ps -o ppid= -p "$(cat "$root/bpf.pid")" | tr -d '[:space:]')
    kill -TERM "$blk" 2>/dev/null || true
    tries=0
    while alive "$blk"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            break
        fi
        sleep 0.1
    done
    if alive "$blk"; then
        bad "$name" "blk-run $blk still running 3s after TERM during sleep"
    elif ! alive "$(cat "$root/bpf.pid")"; then
        ok "$name"
    else
        bad "$name" "bpftrace still running after TERM"
    fi
    kill -9 "$blk" "$(cat "$root/bpf.pid" 2>/dev/null)" "$outer" 2>/dev/null || true
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_blk_run_term_does_not_wait_out_the_snapshot() {
    local name=blk-run-term-snap root err dev outer blk snap tries
    root=$(tmpdir)
    err=$root/err
    dev=$(block_dev) || {
        bad "$name" "no loop0 or dm-0 block device"
        rm -rf "$root"
        return
    }
    prepare_blk_run_fakes "$root" "$dev"
    hold_bpftrace "$root"
    cat >"$root/extent-snap.sh" <<EOF
#!/bin/bash
n=\$(cat "$root/snap.count" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/snap.count"
printf '%s\n' "\$\$" > "$root/snap.pid"
if [ "\$n" -eq 1 ]; then
    exec /bin/sleep 30
fi
exit 0
EOF
    chmod +x "$root/extent-snap.sh"
    PATH="$root/bin:$PATH" with_binds "$root/sys" "$root/extent-snap.sh" "$here/extent-snap.sh" \
        "$here/blk-run.sh" "$dev" "$root/safe/loadtest" "$root/trace" >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/snap.pid" || ! wait_file "$root/bpf.pid"; then
        bad "$name" "snapshot did not start: $(cat "$err" 2>/dev/null)"
        kill "$outer" 2>/dev/null || true
        wait "$outer" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    blk=$(ps -o ppid= -p "$(cat "$root/bpf.pid")" | tr -d '[:space:]')
    snap=$(cat "$root/snap.pid")
    kill -TERM "$blk" 2>/dev/null || true
    tries=0
    while alive "$blk" || alive "$snap"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            break
        fi
        sleep 0.1
    done
    if alive "$blk"; then
        bad "$name" "blk-run $blk still running 3s after TERM during snapshot"
    elif alive "$snap"; then
        bad "$name" "snapshot $snap still running after TERM"
    else
        ok "$name"
    fi
    kill -9 "$blk" "$snap" "$(cat "$root/bpf.pid" 2>/dev/null)" "$outer" 2>/dev/null || true
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_blk_run_snapshots_when_bpftrace_exits() {
    local name=blk-run-snap-exit root err dev outer
    root=$(tmpdir)
    err=$root/err
    dev=$(block_dev) || {
        bad "$name" "no loop0 or dm-0 block device"
        rm -rf "$root"
        return
    }
    prepare_blk_run_fakes "$root" "$dev"
    cat >"$root/bin/bpftrace" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bpf.pid"
exec /bin/sleep 1.2
EOF
    cat >"$root/bin/sleep" <<'EOF'
#!/bin/bash
if [ "$1" = "30" ]; then
    exec /bin/sleep 0.05
fi
exec /bin/sleep "$@"
EOF
    cat >"$root/extent-snap.sh" <<EOF
#!/bin/bash
n=\$(cat "$root/snap.count" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/snap.count"
exit 0
EOF
    chmod +x "$root/bin/bpftrace" "$root/bin/sleep" "$root/extent-snap.sh"
    if ! PATH="$root/bin:$PATH" with_binds "$root/sys" "$root/extent-snap.sh" "$here/extent-snap.sh" \
        "$here/blk-run.sh" "$dev" "$root/safe/loadtest" "$root/trace" >"$err" 2>&1; then
        bad "$name" "blk-run failed: $(cat "$err")"
    elif [ ! -s "$root/snap.count" ] || [ "$(cat "$root/snap.count")" -lt 1 ]; then
        bad "$name" "no snapshot was taken"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# iostat on PATH that stays up until TERM and records the queue value it saw.
# GNU sleep dies on TERM too, but it cannot record that value.
install_iostat_holder() {
    local root=$1 queue=$2
    mkdir -p "$root/bin"
    cat >"$root/bin/iostat.py" <<'PY'
import os
import signal
import sys
import time

root = sys.argv[1]
queue = sys.argv[2]


def stop(_signum, _frame):
    path = os.path.join(queue, "max_sectors_kb")
    try:
        data = open(path, encoding="utf-8").read()
    except OSError:
        data = ""
    with open(os.path.join(root, "iostat.saw"), "w", encoding="utf-8") as handle:
        handle.write(data)
    sys.exit(0)


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
with open(os.path.join(root, "iostat.pid"), "w", encoding="utf-8") as handle:
    handle.write(f"{os.getpid()}\n")
with open(os.path.join(root, "iostat.ppid"), "w", encoding="utf-8") as handle:
    handle.write(f"{os.getppid()}\n")
time.sleep(60)
PY
    cat >"$root/bin/iostat" <<EOF
#!/bin/bash
exec python3 "$root/bin/iostat.py" "$root" "$queue"
EOF
    chmod +x "$root/bin/iostat"
}

stop_pids() {
    local pid
    for pid in "$@"; do
        if [ -n "$pid" ] && alive "$pid"; then
            kill -9 "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
}

test_run_records_the_sampler_and_stops_it_when_the_bench_exits() {
    local name=run-sample-exit root err dev mnt queue
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    queue=$root/sys/block/$dev/queue
    install_iostat_holder "$root" "$queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
i=0
while [ ! -s "$root/iostat.pid" ]; do
    i=\$((i + 1))
    if [ "\$i" -ge 50 ]; then
        exit 1
    fi
    sleep 0.1
done
if [ -s "$root/out/t/sample.pid" ]; then
    printf 'yes\n' > "$root/sample-before-bench"
fi
printf '%s' irys-sm-bench > /proc/\$\$/comm
exit 0
EOF
    chmod +x "$root/bench"
    PATH="$root/bin:$PATH" with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1 &
    outer=$!
    local tries=0
    while alive "$outer"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 100 ]; then
            break
        fi
        sleep 0.1
    done
    if alive "$outer"; then
        bad "$name" "run.sh still running after the bench exited: $(cat "$err" 2>/dev/null)"
        kill -9 "$outer" 2>/dev/null || true
        wait "$outer" 2>/dev/null || true
    elif ! wait "$outer"; then
        bad "$name" "run.sh failed: $(cat "$err" 2>/dev/null)"
    elif [ ! -f "$root/sample-before-bench" ]; then
        bad "$name" "sample.pid was not written before the bench: $(cat "$err" 2>/dev/null)"
    elif [ ! -s "$root/iostat.pid" ]; then
        bad "$name" "iostat did not start"
    elif alive "$(cat "$root/iostat.pid")"; then
        bad "$name" "iostat still running after the bench exited"
    elif [ -e "$root/out/t/run.pid" ] || [ -e "$root/out/t/sample.pid" ]; then
        bad "$name" "pid files remain: run=$(cat "$root/out/t/run.pid" 2>/dev/null) sample=$(cat "$root/out/t/sample.pid" 2>/dev/null)"
    else
        ok "$name"
    fi
    if [ -s "$root/iostat.pid" ]; then
        stop_pids "$(cat "$root/iostat.pid")" "$(cat "$root/iostat.ppid" 2>/dev/null || true)"
    fi
    rm -rf "$root"
}

test_teardown_stops_the_sampler_after_run_is_killed() {
    local name=teardown-after-kill9 root err dev mnt queue outer
    local bench_pid sample_pid io_pid
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    queue=$root/sys/block/$dev/queue
    install_iostat_holder "$root" "$queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
printf '%s' irys-sm-bench > /proc/\$\$/comm
printf '%s\n' "\$\$" > "$root/bench.pid"
sleep 60
EOF
    chmod +x "$root/bench"
    PATH="$root/bin:$PATH" with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/bench.pid" || ! wait_file "$root/out/t/run.pid" || ! wait_file "$root/iostat.pid"; then
        bad "$name" "run did not start: $(cat "$err" 2>/dev/null)"
        stop_pids "$outer"
        rm -rf "$root"
        return
    fi
    bench_pid=$(cat "$root/bench.pid")
    sample_pid=$(cat "$root/iostat.ppid")
    io_pid=$(cat "$root/iostat.pid")
    # $! is the function subshell. sample.sh's parent is run.sh.
    run_shell=$(ps -o ppid= -p "$sample_pid" | tr -d '[:space:]')
    cmd=$(tr '\0' ' ' <"/proc/$run_shell/cmdline" 2>/dev/null || true)
    case "$cmd" in
        *run.sh*) ;;
        *)
            bad "$name" "sample parent $run_shell is not run.sh: $cmd"
            stop_pids "$outer" "$bench_pid" "$sample_pid" "$io_pid"
            rm -rf "$root"
            return
            ;;
    esac
    kill -9 "$run_shell" 2>/dev/null || true
    wait "$outer" 2>/dev/null || true
    if ! alive "$bench_pid" || ! alive "$sample_pid" || ! alive "$io_pid"; then
        bad "$name" "kill -9 of run.sh reaped bench=$bench_pid sample=$sample_pid iostat=$io_pid"
        stop_pids "$bench_pid" "$sample_pid" "$io_pid"
        rm -rf "$root"
        return
    fi
    printf '999\n' >"$queue/max_sectors_kb"
    if ! with_fake_sys "$root/sys" "$here/teardown.sh" "$dev" "$root/out/t/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown failed: $(cat "$err")"
    elif alive "$io_pid"; then
        bad "$name" "iostat $io_pid still running after teardown"
    elif alive "$sample_pid"; then
        bad "$name" "sample.sh $sample_pid still running after teardown"
    elif alive "$bench_pid"; then
        bad "$name" "bench $bench_pid still running after teardown"
    elif [ "$(cat "$root/iostat.saw" 2>/dev/null | tr -d '[:space:]')" != "999" ]; then
        bad "$name" "iostat saw [$(cat "$root/iostat.saw" 2>/dev/null)] not the pre-restore queue"
    elif ! grep -q '^111$' "$queue/max_sectors_kb"; then
        bad "$name" "queue was not restored: $(cat "$queue/max_sectors_kb")"
    else
        ok "$name"
    fi
    stop_pids "$bench_pid" "$sample_pid" "$io_pid"
    rm -rf "$root"
}

# Missing, non-numeric, and empty sample.pid values still restore the queue.
test_teardown_restores_for_a_harmless_sample_pid() {
    local spec name mode payload root err
    local -a rows=(
        'teardown-sample-missing|absent|teardown refused a missing sample.pid'
        'teardown-sample-bad|nope|teardown refused a non-numeric sample.pid'
        'teardown-sample-empty|empty|teardown refused an empty sample.pid'
    )
    for spec in "${rows[@]}"; do
        IFS='|' read -r name mode payload <<<"$spec"
        root=$(tmpdir)
        err=$root/err
        queue_restore_fixture "$root"
        printf '%s\n' "$(finished_pid)" >"$root/run/run.pid"
        case "$mode" in
            absent) ;;
            nope) printf 'nope\n' >"$root/run/sample.pid" ;;
            empty) : >"$root/run/sample.pid" ;;
        esac
        if ! with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
            bad "$name" "$payload: $(cat "$err")"
        elif ! grep -q '^111$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
            bad "$name" "queue was not restored"
        else
            ok "$name"
        fi
        rm -rf "$root"
    done
}

test_teardown_does_not_signal_a_sample_pid_that_is_not_the_sampler() {
    local name=teardown-sample-other root err decoy
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    printf '%s\n' "$(finished_pid)" >"$root/run/run.pid"
    sleep 60 &
    decoy=$!
    printf '%s\n' "$decoy" >"$root/run/sample.pid"
    if ! with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown failed: $(cat "$err")"
    elif ! alive "$decoy"; then
        bad "$name" "pid $decoy was signaled though it is not sample.sh"
    elif ! grep -q '^111$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue was not restored"
    else
        ok "$name"
    fi
    stop_pids "$decoy"
    rm -rf "$root"
}

test_teardown_does_not_stop_the_sampler_when_the_bench_pid_is_wrong() {
    local name=teardown-sample-after-bench-check root err queue sample_pid io_pid sleeper
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    queue=$root/sys/block/notadisk/queue
    install_iostat_holder "$root" "$queue"
    sleep 60 &
    sleeper=$!
    printf '%s\n' "$sleeper" >"$root/run/run.pid"
    PATH="$root/bin:$PATH" "$here/sample.sh" notadisk "$root/run" >"$root/sample.err" 2>&1 &
    sample_pid=$!
    if ! wait_file "$root/iostat.pid"; then
        bad "$name" "sampler did not start: $(cat "$root/sample.err" 2>/dev/null)"
        stop_pids "$sleeper" "$sample_pid"
        rm -rf "$root"
        return
    fi
    io_pid=$(cat "$root/iostat.pid")
    printf '%s\n' "$sample_pid" >"$root/run/sample.pid"
    if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown exited 0 for a pid whose comm is not the bench"
    elif ! alive "$sample_pid" || ! alive "$io_pid"; then
        bad "$name" "sampler was signaled before the bench check"
    elif ! grep -q '^999$' "$queue/max_sectors_kb"; then
        bad "$name" "queue changed"
    else
        ok "$name"
    fi
    stop_pids "$sleeper" "$sample_pid" "$io_pid"
    rm -rf "$root"
}

low_df() {
    local root=$1
    mkdir -p "$root/bin"
    cat >"$root/bin/df" <<'EOF'
#!/bin/bash
if [ "$1" = "-B1" ]; then
    printf 'Avail\n1024\n'
    exit 0
fi
exec /usr/bin/df "$@"
EOF
    chmod +x "$root/bin/df"
}

test_run_refuses_a_negative_min_free_gib() {
    local name=run-min-free-negative root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib -1 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran below a negative floor"
    elif ! grep -q "min free GiB" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_an_overflowing_min_free_gib() {
    local name=run-min-free-overflow root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 8589934592 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran below a wrapped floor"
    elif ! grep -q "min free GiB" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_a_negative_min_free_gib_from_the_environment() {
    local name=run-min-free-env root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if MIN_FREE_GIB=-1 with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "min free GiB" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_does_not_stash_when_free_space_is_below_the_floor() {
    local name=ladder-free-floor root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    low_df "$root"
    if PATH="$root/bin:$PATH" with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --max-sectors-kb 64 --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "below" "$err"; then
        bad "$name" "stderr did not refuse free space: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was moved"
    elif ! grep -q '^111$' "$sys/block/$dev/queue/max_sectors_kb"; then
        bad "$name" "queue value changed: $(cat "$sys/block/$dev/queue/max_sectors_kb")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_restores_state_when_run_refuses_after_stash() {
    local name=ladder-restore-after-refuse root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    if [ "$mnt" = "/workspaces" ]; then
        bad "$name" "loadtest mount is /workspaces; this case needs two mounts"
        rm -rf "$root"
        return
    fi
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount /workspaces --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "not the loadtest mount" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index state was not restored"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_stashes_before_a_run_that_starts() {
    local name=ladder-stash-on-success root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh refused a run: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif [ ! -f "$module/state-prior/db" ]; then
        bad "$name" "prior index was not stashed"
    elif [ -e "$module/db" ]; then
        bad "$name" "db stayed in the module root"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "stashed db bytes changed"
    elif [ ! -f "$module/chunks.dat" ]; then
        bad "$name" "chunks.dat was moved"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# Kill after the first rename: db is only in state-prior, the other index
# files are still live. The restart puts db back and the run stashes one
# complete index. chunks.dat is not a stashed name.
test_ladder_reunites_a_stash_that_stopped_between_renames() {
    local name=ladder-partial-stash root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    mv "$module/db" "$module/state-prior/db"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh refused a partial stash: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif [ -e "$module/db" ] || [ -e "$module/intervals.json" ]; then
        bad "$name" "live index was not stashed after the repair"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "db was not kept: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/state-prior/intervals.json"; then
        bad "$name" "intervals were not stashed with db"
    elif ! cmp -s <(printf 'keep-chunks.dat\n') "$module/chunks.dat"; then
        bad "$name" "chunks.dat changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# Kill after mkdir and before any rename. The empty directory is not an index.
test_ladder_continues_when_the_state_dir_is_empty() {
    local name=ladder-empty-state-dir root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh refused an empty state dir: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif [ -e "$module/db" ]; then
        bad "$name" "db stayed in the module root"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "stashed db changed: $(cat "$module/state-prior/db" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A name in both places stays an error, and nothing is moved. The error names
# a file that exists only in the state directory.
test_ladder_names_files_left_only_in_an_existing_state_dir() {
    local name=ladder-state-only-names root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    printf 'OLD\n' >"$module/state-prior/db"
    mv "$module/rocks" "$module/state-prior/rocks"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "already exists" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! grep -q "only in $module/state-prior: rocks" "$err"; then
        bad "$name" "stderr did not name the stranded file: $(cat "$err")"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "live db changed: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'OLD\n') "$module/state-prior/db"; then
        bad "$name" "state db changed: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-rocks\n') "$module/state-prior/rocks"; then
        bad "$name" "stranded rocks changed or moved"
    elif [ -e "$module/rocks" ]; then
        bad "$name" "rocks was copied back beside the conflicting db"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/intervals.json"; then
        bad "$name" "live intervals changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# An unknown file stays in the state directory. The stranded index name is
# put back, and the bench does not start while that file is still there.
test_ladder_keeps_an_unknown_file_in_a_partial_state_dir() {
    local name=ladder-partial-unknown-file root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    mv "$module/db" "$module/state-prior/db"
    printf 'note\n' >"$module/state-prior/note"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "already exists" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "db was not put back: $(cat "$module/db" 2>/dev/null)"
    elif [ -e "$module/state-prior/db" ]; then
        bad "$name" "db is still only in the state directory"
    elif ! cmp -s <(printf 'note\n') "$module/state-prior/note"; then
        bad "$name" "unknown file was removed"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/intervals.json"; then
        bad "$name" "live intervals changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A completed stash leaves no live index names. That restart still runs, and
# it does not pull the stashed index back.
test_ladder_starts_when_the_live_index_is_already_absent() {
    local name=ladder-stashed-index-stays root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    local item
    for item in db rocks intervals.json packing_params.toml bench-prepop.json; do
        mv "$module/$item" "$module/state-prior/$item"
    done
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh refused a completed stash: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif [ -e "$module/db" ]; then
        bad "$name" "stashed db was moved back"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "stashed db changed"
    elif ! cmp -s <(printf 'keep-chunks.dat\n') "$module/chunks.dat"; then
        bad "$name" "chunks.dat changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_does_not_restore_a_state_dir_it_did_not_create() {
    local name=ladder-keep-existing-state root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    mkdir -p "$module/state-prior"
    printf 'OLD\n' >"$module/state-prior/db"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "already exists" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "live db changed: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'OLD\n') "$module/state-prior/db"; then
        bad "$name" "existing state dir changed: $(cat "$module/state-prior/db" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# findmnt names a mapper node this container does not have. dm-0 is that disk.
snap_findmnt() {
    local root=$1
    mkdir -p "$root/bin"
    cat >"$root/bin/findmnt" <<'EOF'
#!/bin/bash
case "$*" in
    *SOURCE*) printf '%s\n' "/dev/dm-0" ;;
    *) printf '%s\n' "/workspaces" ;;
esac
EOF
    chmod +x "$root/bin/findmnt"
}

test_extent_snap_writes_a_snapshot() {
    local name=extent-snap-ok root mnt
    # /tmp is tmpfs ("none") and has no device node. The snapshot needs one.
    root=$(other_tmpdir)
    mnt=$(mount_of "$root")
    mkdir -p "$root/data" "$root/out"
    printf 'x\n' >"$root/data/file"
    snap_findmnt "$root"
    if ! PATH="$root/bin:$PATH" "$here/extent-snap.sh" "$mnt" "$root/data" "$root/out" >"$root/err" 2>&1; then
        bad "$name" "extent-snap failed: $(cat "$root/err")"
    elif ! compgen -G "$root/out/extents-*.txt" >/dev/null; then
        bad "$name" "no extents file: $(cat "$root/err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_extent_snap_term_stops_the_walk() {
    local name=extent-snap-term root mnt snap walk tries
    # /tmp is tmpfs ("none") and has no device node. The snapshot needs one.
    root=$(other_tmpdir)
    mnt=$(mount_of "$root")
    mkdir -p "$root/bin" "$root/data" "$root/out"
    printf 'x\n' >"$root/data/file"
    snap_findmnt "$root"
    cat >"$root/bin/python3" <<EOF
#!/bin/bash
if [ "\$1" = "-c" ]; then
    exec /usr/bin/python3 "\$@"
fi
printf '%s\n' "\$\$" > "$root/py.pid"
sleep 60 &
printf '%s\n' "\$!" > "$root/walk.pid"
wait
EOF
    chmod +x "$root/bin/python3"
    PATH="$root/bin:$PATH" "$here/extent-snap.sh" "$mnt" "$root/data" "$root/out" >"$root/err" 2>&1 &
    snap=$!
    if ! wait_file "$root/walk.pid"; then
        bad "$name" "walk did not start: $(cat "$root/err" 2>/dev/null)"
        kill "$snap" 2>/dev/null || true
        wait "$snap" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    walk=$(cat "$root/walk.pid")
    kill -TERM "$snap" 2>/dev/null || true
    tries=0
    while alive "$snap" || alive "$walk"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            break
        fi
        sleep 0.1
    done
    if alive "$walk"; then
        bad "$name" "walk $walk still running after TERM"
    elif alive "$snap"; then
        bad "$name" "extent-snap $snap still running after TERM"
    else
        ok "$name"
    fi
    kill -9 "$snap" "$walk" 2>/dev/null || true
    wait "$snap" 2>/dev/null || true
    rm -rf "$root"
}

test_blk_run_term_stops_the_snapshot_child() {
    local name=blk-run-term-snap-child root err dev outer blk snap child tries
    root=$(tmpdir)
    err=$root/err
    dev=$(block_dev) || {
        bad "$name" "no loop0 or dm-0 block device"
        rm -rf "$root"
        return
    }
    prepare_blk_run_fakes "$root" "$dev"
    hold_bpftrace "$root"
    cat >"$root/extent-snap.sh" <<EOF
#!/bin/bash
n=\$(cat "$root/snap.count" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/snap.count"
printf '%s\n' "\$\$" > "$root/snap.pid"
if [ "\$n" -eq 1 ]; then
    sleep 60 &
    printf '%s\n' "\$!" > "$root/child.pid"
    wait
fi
exit 0
EOF
    chmod +x "$root/extent-snap.sh"
    PATH="$root/bin:$PATH" with_binds "$root/sys" "$root/extent-snap.sh" "$here/extent-snap.sh" \
        "$here/blk-run.sh" "$dev" "$root/safe/loadtest" "$root/trace" >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/child.pid" || ! wait_file "$root/bpf.pid"; then
        bad "$name" "snapshot child did not start: $(cat "$err" 2>/dev/null)"
        kill "$outer" 2>/dev/null || true
        wait "$outer" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    blk=$(ps -o ppid= -p "$(cat "$root/bpf.pid")" | tr -d '[:space:]')
    snap=$(cat "$root/snap.pid")
    child=$(cat "$root/child.pid")
    local pgid
    pgid=$(ps -o pgid= -p "$snap" | tr -d '[:space:]')
    kill -TERM "$blk" 2>/dev/null || true
    tries=0
    while alive "$blk" || alive "$snap" || alive "$child"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            break
        fi
        sleep 0.1
    done
    if [ "$pgid" != "$snap" ]; then
        bad "$name" "snapshot pgid $pgid is not its pid $snap"
    elif alive "$blk"; then
        bad "$name" "blk-run $blk still running after TERM"
    elif alive "$child"; then
        bad "$name" "snapshot child $child still running after TERM"
    elif alive "$snap"; then
        bad "$name" "snapshot $snap still running after TERM"
    else
        ok "$name"
    fi
    kill -9 "$blk" "$snap" "$child" "$(cat "$root/bpf.pid" 2>/dev/null)" "$outer" 2>/dev/null || true
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

# Cleanup ignores TERM on the shell, then waits for the farewell snapshot.
# A second TERM to the blk-run pid must still stop that snapshot and its child.
test_blk_run_second_term_stops_the_farewell_snapshot() {
    local name=blk-run-term-farewell root err dev outer blk snap child tries
    root=$(tmpdir)
    err=$root/err
    dev=$(block_dev) || {
        bad "$name" "no loop0 or dm-0 block device"
        rm -rf "$root"
        return
    }
    prepare_blk_run_fakes "$root" "$dev"
    hold_bpftrace "$root"
    cat >"$root/extent-snap.sh" <<EOF
#!/bin/bash
n=\$(cat "$root/snap.count" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/snap.count"
printf '%s\n' "\$\$" > "$root/snap.pid"
if [ "\$n" -ge 2 ]; then
    sleep 60 &
    printf '%s\n' "\$!" > "$root/child.pid"
    wait
fi
exit 0
EOF
    chmod +x "$root/extent-snap.sh"
    PATH="$root/bin:$PATH" with_binds "$root/sys" "$root/extent-snap.sh" "$here/extent-snap.sh" \
        "$here/blk-run.sh" "$dev" "$root/safe/loadtest" "$root/trace" >"$err" 2>&1 &
    outer=$!
    if ! wait_file "$root/snap.pid" || ! wait_file "$root/bpf.pid"; then
        bad "$name" "startup snapshot did not start: $(cat "$err" 2>/dev/null)"
        kill "$outer" 2>/dev/null || true
        wait "$outer" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    blk=$(ps -o ppid= -p "$(cat "$root/bpf.pid")" | tr -d '[:space:]')
    tries=0
    while alive "$(cat "$root/snap.pid")"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            bad "$name" "startup snapshot did not exit"
            kill -9 "$blk" "$outer" 2>/dev/null || true
            wait "$outer" 2>/dev/null || true
            rm -rf "$root"
            return
        fi
        sleep 0.1
    done
    if ! alive "$blk"; then
        bad "$name" "blk-run exited before the farewell snapshot: $(cat "$err")"
        wait "$outer" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    kill -TERM "$blk" 2>/dev/null || true
    if ! wait_file "$root/child.pid"; then
        bad "$name" "farewell snapshot did not start a child: count=$(cat "$root/snap.count" 2>/dev/null) $(cat "$err")"
        kill -9 "$blk" "$outer" 2>/dev/null || true
        wait "$outer" 2>/dev/null || true
        rm -rf "$root"
        return
    fi
    snap=$(cat "$root/snap.pid")
    child=$(cat "$root/child.pid")
    kill -TERM "$blk" 2>/dev/null || true
    tries=0
    while alive "$blk" || alive "$snap" || alive "$child"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 30 ]; then
            break
        fi
        sleep 0.1
    done
    if alive "$child"; then
        bad "$name" "farewell child $child still running 3s after the second TERM"
    elif alive "$snap"; then
        bad "$name" "farewell snapshot $snap still running after the second TERM"
    elif alive "$blk"; then
        bad "$name" "blk-run $blk still running after the second TERM"
    elif [ "$(cat "$root/snap.count")" -lt 2 ]; then
        bad "$name" "farewell snapshot did not run: count=$(cat "$root/snap.count")"
    else
        ok "$name"
    fi
    kill -9 "$blk" "$snap" "$child" "$(cat "$root/bpf.pid" 2>/dev/null)" "$outer" 2>/dev/null || true
    wait "$outer" 2>/dev/null || true
    rm -rf "$root"
}

test_run_exits_1_when_the_bench_exits_1() {
    local name=run-bench-exit-1 root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 1\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -ne 1 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    elif ! grep -q '^exit=1$' "$root/out/t/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/t/bench.out" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_exits_0_when_the_bench_exits_2() {
    local name=run-bench-exit-2 root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    cat >"$root/bench" <<EOF
#!/bin/sh
mkdir -p "$root/out/t"
printf '{}\n' > "$root/out/t/header.json"
printf ran > "$root/bench-ran"
exit 2
EOF
    chmod +x "$root/bench"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited non-zero for a scored miss: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif ! grep -q '^exit=2$' "$root/out/t/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/t/bench.out" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A usage error is also exit 2. It does not write header.json. That is not a
# scored miss, so run.sh must keep status 2.
test_run_exits_2_when_exit_2_writes_no_header() {
    local name=run-exit-2-no-header root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 2\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "run.sh treated a usage error as a scored miss: $(cat "$err")"
    elif [ "$rc" -ne 2 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    elif [ -e "$root/out/t/header.json" ]; then
        bad "$name" "header.json exists"
    elif ! grep -q '^exit=2$' "$root/out/t/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/t/bench.out" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A previous run's header.json is not this run's scored miss. Exit 2 without
# rewriting that file stays exit 2, and the old file stays.
test_run_keeps_exit_2_when_an_old_header_is_not_rewritten() {
    local name=run-exit-2-old-header root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/out/t"
    printf 'OLD-HEADER\n' >"$root/out/t/header.json"
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 2\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "run.sh treated the old header as a scored miss: $(cat "$err")"
    elif [ "$rc" -ne 2 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif ! cmp -s <(printf 'OLD-HEADER\n') "$root/out/t/header.json"; then
        bad "$name" "header.json: $(cat "$root/out/t/header.json" 2>/dev/null)"
    elif [ -e "$root/out/t/header.json.prior" ]; then
        bad "$name" "header.json.prior was left behind"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# header.json.prior is a second name for header.json. mv fails, the old
# file stays, and exit 2 must not count as a scored miss. The bench must
# not start.
test_run_exits_1_when_the_header_aside_is_the_same_file() {
    local name=run-header-aside-same-file root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/out/t"
    printf 'OLD-SCORE\n' >"$root/out/t/header.json"
    ln "$root/out/t/header.json" "$root/out/t/header.json.prior"
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 2\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "run.sh treated the old header as a scored miss: $(cat "$err")"
    elif [ "$rc" -ne 1 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran after the aside failed: $(cat "$err")"
    elif ! cmp -s <(printf 'OLD-SCORE\n') "$root/out/t/header.json"; then
        bad "$name" "header.json: $(cat "$root/out/t/header.json" 2>/dev/null)"
    elif ! grep -q "cannot move" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# The aside path is a directory mounted read-only. mv cannot put the old
# header there. Exit 2 must not count as a scored miss, and the bench must
# not start. The trap's rm of that directory must not choose the status.
test_run_exits_1_when_the_header_aside_directory_is_not_writable() {
    local name=run-header-aside-ro root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/out/t/header.json.prior"
    printf 'OLD-SCORE\n' >"$root/out/t/header.json"
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 2\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    cat >"$root/wrap" <<EOF
#!/bin/bash
mount --bind "$root/out/t/header.json.prior" "$root/out/t/header.json.prior" || exit 96
mount -o remount,bind,ro "$root/out/t/header.json.prior" || exit 95
exec "$here/run.sh" "\$@"
EOF
    chmod +x "$root/wrap"
    with_fake_sys "$root/sys" "$root/wrap" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "run.sh treated the old header as a scored miss: $(cat "$err")"
    elif [ "$rc" -ne 1 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran after the aside failed: $(cat "$err")"
    elif ! cmp -s <(printf 'OLD-SCORE\n') "$root/out/t/header.json"; then
        bad "$name" "header.json: $(cat "$root/out/t/header.json" 2>/dev/null)"
    elif [ -e "$root/out/t/header.json.prior/header.json" ]; then
        bad "$name" "mv wrote into the read-only aside"
    elif ! grep -q "cannot move" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A scored miss writes a new header. header.json.prior is a directory, so
# the trap cannot remove it. That rm must not turn the scored exit into 1.
test_run_exits_0_when_exit_2_replaces_a_header_and_the_aside_is_a_directory() {
    local name=run-exit-2-aside-directory root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/out/t/header.json.prior"
    printf 'OLD-SCORE\n' >"$root/out/t/header.json"
    cat >"$root/bench" <<EOF
#!/bin/sh
printf 'NEW-SCORE\n' > "$root/out/t/header.json"
printf ran > "$root/bench-ran"
exit 2
EOF
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ]; then
        bad "$name" "run.sh exited $rc: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    elif ! cmp -s <(printf 'NEW-SCORE\n') "$root/out/t/header.json"; then
        bad "$name" "header.json: $(cat "$root/out/t/header.json" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A scored miss rewrites header.json. An older file in that path does not
# replace the new one, and the run still counts as success.
test_run_exits_0_when_exit_2_replaces_an_old_header() {
    local name=run-exit-2-replaces-header root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/out/t"
    printf 'OLD-HEADER\n' >"$root/out/t/header.json"
    cat >"$root/bench" <<EOF
#!/bin/sh
printf 'NEW-HEADER\n' > "$root/out/t/header.json"
printf ran > "$root/bench-ran"
exit 2
EOF
    chmod +x "$root/bench"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a scored miss that rewrote header.json: $(cat "$err")"
    elif ! cmp -s <(printf 'NEW-HEADER\n') "$root/out/t/header.json"; then
        bad "$name" "header.json: $(cat "$root/out/t/header.json" 2>/dev/null)"
    elif [ -e "$root/out/t/header.json.prior" ]; then
        bad "$name" "header.json.prior was left behind"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# 101 is a panic. 0 and 2 stay success. Every other status is a failure.
test_run_exits_nonzero_when_the_bench_exits_101() {
    local name=run-bench-exit-101 root err dev mnt rc
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 101\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "run.sh exited 0: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$err")"
    elif ! grep -q '^exit=101$' "$root/out/t/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/t/bench.out" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_restores_the_index_when_the_bench_exits_1() {
    local name=ladder-bench-exit-1 root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 1\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not start: $(cat "$err")"
    elif ! state_in_place "$module"; then
        bad "$name" "index was not restored"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "db bytes changed: $(cat "$module/db" 2>/dev/null)"
    elif [ -e "$module/state-prior" ]; then
        bad "$name" "state-prior still exists"
    elif ! grep -q '^exit=1$' "$root/out/only-run/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/only-run/bench.out" 2>/dev/null)"
    elif grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker says success: $(cat "$root/out/ladder.done")"
    elif ! grep -q 'run only-run failed' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_leaves_a_new_db_when_exit_1_created_it() {
    local name=ladder-exit-1-keeps-new-db root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/sh
printf 'new-db\n' > "$module/db"
printf ran > "$root/bench-ran"
exit 1
EOF
    chmod +x "$root/bench"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not start: $(cat "$err")"
    elif ! cmp -s <(printf 'new-db\n') "$module/db"; then
        bad "$name" "new db was replaced: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "prior db changed: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker says success: $(cat "$root/out/ladder.done")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# The bench wrote intervals.json and packing_params.toml, then exited 1.
# db and rocks are only in the state directory. A restore must move nothing,
# and it must name every name that is in both places. The next start still
# moves nothing.
test_ladder_does_not_move_a_restore_when_a_name_is_in_both_places() {
    local name=ladder-restore-conflict root module sys err err2 dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    err2=$root/err2
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/sh
printf 'NEW-ENTROPY\n' > "$module/intervals.json"
printf 'NEW-PARAMS\n' > "$module/packing_params.toml"
printf ran > "$root/bench-ran"
exit 1
EOF
    chmod +x "$root/bench"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
        rm -rf "$root"
        return
    fi
    if [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not start: $(cat "$err")"
    elif [ -e "$module/db" ] || [ -e "$module/rocks" ] || [ -e "$module/bench-prepop.json" ]; then
        bad "$name" "a name that was only in the state directory was restored"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "state db: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-rocks\n') "$module/state-prior/rocks"; then
        bad "$name" "state rocks changed or moved"
    elif ! cmp -s <(printf 'keep-bench-prepop.json\n') "$module/state-prior/bench-prepop.json"; then
        bad "$name" "state bench-prepop changed or moved"
    elif ! cmp -s <(printf 'NEW-ENTROPY\n') "$module/intervals.json"; then
        bad "$name" "live intervals: $(cat "$module/intervals.json" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/state-prior/intervals.json"; then
        bad "$name" "state intervals changed or moved"
    elif ! cmp -s <(printf 'NEW-PARAMS\n') "$module/packing_params.toml"; then
        bad "$name" "live packing params: $(cat "$module/packing_params.toml" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-packing_params.toml\n') "$module/state-prior/packing_params.toml"; then
        bad "$name" "state packing params changed or moved"
    elif ! cmp -s <(printf 'keep-chunks.dat\n') "$module/chunks.dat"; then
        bad "$name" "chunks.dat changed"
    elif ! grep -q 'intervals.json' "$err" || ! grep -q 'packing_params.toml' "$err"; then
        bad "$name" "stderr did not name both conflicts: $(cat "$err")"
    else
        rm -f "$root/bench-ran"
        if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
            --device "$dev" --mount "$mnt" --loadtest "$module" \
            --out-root "$root/out" --bench "$root/bench" --run only-run \
            >"$err2" 2>&1; then
            bad "$name" "retry exited 0: $(cat "$err2")"
        elif [ -e "$root/bench-ran" ]; then
            bad "$name" "retry ran the bench"
        elif [ -e "$module/db" ]; then
            bad "$name" "retry restored db"
        elif ! cmp -s <(printf 'NEW-ENTROPY\n') "$module/intervals.json"; then
            bad "$name" "retry changed live intervals"
        elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
            bad "$name" "retry changed state db"
        elif ! grep -q 'already exists' "$err2"; then
            bad "$name" "retry stderr: $(cat "$err2")"
        elif ! grep -q 'db' "$err2"; then
            bad "$name" "retry did not name the stranded db: $(cat "$err2")"
        else
            ok "$name"
        fi
    fi
    rm -rf "$root"
}

# db, rocks, and intervals.json move, then the next mv fails. Those three go
# back to the state directory. The live module does not keep a partial index.
test_ladder_puts_back_names_when_a_later_restore_move_fails() {
    local name=ladder-restore-mv-fails root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue" "$root/bin"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bin/mv" <<EOF
#!/bin/bash
if [ "\${1-}" = "$module/state-prior/packing_params.toml" ]; then
    echo "injected mv failure" >&2
    exit 1
fi
exec /bin/mv "\$@"
EOF
    chmod +x "$root/bin/mv"
    printf '#!/bin/sh\nprintf ran >"%s/bench-ran"\nexit 1\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    if PATH="$root/bin:$PATH" MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not start: $(cat "$err")"
    elif ! grep -q 'injected mv failure' "$err"; then
        bad "$name" "mv was not the failure: $(cat "$err")"
    elif [ -e "$module/db" ] || [ -e "$module/rocks" ] || [ -e "$module/intervals.json" ]; then
        bad "$name" "a restored name stayed in the module root"
    elif [ -e "$module/packing_params.toml" ] || [ -e "$module/bench-prepop.json" ]; then
        bad "$name" "a later name was restored after the failed move"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "state db: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-rocks\n') "$module/state-prior/rocks"; then
        bad "$name" "state rocks: $(cat "$module/state-prior/rocks" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/state-prior/intervals.json"; then
        bad "$name" "state intervals: $(cat "$module/state-prior/intervals.json" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-packing_params.toml\n') "$module/state-prior/packing_params.toml"; then
        bad "$name" "state packing params moved"
    elif ! cmp -s <(printf 'keep-bench-prepop.json\n') "$module/state-prior/bench-prepop.json"; then
        bad "$name" "state bench-prepop moved"
    elif ! cmp -s <(printf 'keep-chunks.dat\n') "$module/chunks.dat"; then
        bad "$name" "chunks.dat changed"
    elif [ ! -d "$module/state-prior" ]; then
        bad "$name" "state directory was removed after a failed restore"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# Exit 0 still continues. The next step runs and the first index stays stashed.
test_ladder_runs_the_next_step_when_the_bench_exits_0() {
    local name=ladder-bench-exit-0-continues root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
n=\$(cat "$root/calls" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/calls"
if [ "\$n" -eq 2 ]; then
    printf 'second-db\n' > "$module/db"
fi
exit 0
EOF
    chmod +x "$root/bench"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --run first-step --run later-step \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh stopped after exit 0: $(cat "$err")"
    elif [ "$(cat "$root/calls" 2>/dev/null)" != 2 ]; then
        bad "$name" "second step did not run: calls=$(cat "$root/calls" 2>/dev/null)"
    elif ! cmp -s <(printf 'second-db\n') "$module/db"; then
        bad "$name" "live db: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "prior db: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A panic must not look like success. The next step does not start, and the
# index moved aside for the first step comes back.
test_ladder_restores_the_index_when_the_bench_exits_101() {
    local name=ladder-bench-exit-101 root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
n=\$(cat "$root/calls" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/calls"
if [ "\$n" -eq 1 ]; then
    exit 101
fi
printf 'second-db\n' > "$module/db"
exit 0
EOF
    chmod +x "$root/bench"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --run panic-step --run later-step \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ "$(cat "$root/calls" 2>/dev/null)" != 1 ]; then
        bad "$name" "second step ran: calls=$(cat "$root/calls" 2>/dev/null)"
    elif [ -e "$module/state-prior" ]; then
        bad "$name" "state-prior still exists"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "live db: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/intervals.json"; then
        bad "$name" "intervals were not restored"
    elif [ ! -f "$module/chunks.dat" ]; then
        bad "$name" "chunks.dat was moved"
    elif ! grep -q 'run panic-step failed' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    elif grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker says success: $(cat "$root/out/ladder.done")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# Exit 2 with no header.json is a usage error. The ladder restores the index
# and does not start the next step.
test_ladder_restores_the_index_when_exit_2_writes_no_header() {
    local name=ladder-exit-2-no-header root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/bash
n=\$(cat "$root/calls" 2>/dev/null || echo 0)
n=\$((n + 1))
printf '%s\n' "\$n" > "$root/calls"
if [ "\$n" -eq 1 ]; then
    exit 2
fi
printf 'second-db\n' > "$module/db"
exit 0
EOF
    chmod +x "$root/bench"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --run typo-step --run later-step \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ "$(cat "$root/calls" 2>/dev/null)" != 1 ]; then
        bad "$name" "second step ran: calls=$(cat "$root/calls" 2>/dev/null)"
    elif [ -e "$module/state-prior" ]; then
        bad "$name" "state-prior still exists"
    elif ! cmp -s <(printf 'keep-db\n') "$module/db"; then
        bad "$name" "live db: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-intervals.json\n') "$module/intervals.json"; then
        bad "$name" "intervals were not restored"
    elif [ -e "$root/out/typo-step/header.json" ]; then
        bad "$name" "header.json was created"
    elif ! grep -q 'run typo-step failed' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    elif grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker says success: $(cat "$root/out/ladder.done")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_keeps_the_new_index_when_the_bench_exits_2() {
    local name=ladder-bench-exit-2 root module sys err dev mnt
    root=$(tmpdir)
    module=$root/safe/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    cat >"$root/bench" <<EOF
#!/bin/sh
mkdir -p "$root/out/only-run"
printf '{}\n' > "$root/out/only-run/header.json"
printf 'new-db\n' > "$module/db"
printf ran > "$root/bench-ran"
exit 2
EOF
    chmod +x "$root/bench"
    if ! MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh refused a scored miss: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif ! cmp -s <(printf 'new-db\n') "$module/db"; then
        bad "$name" "new db was replaced: $(cat "$module/db" 2>/dev/null)"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "prior db changed: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif ! grep -q '^exit=2$' "$root/out/only-run/bench.out"; then
        bad "$name" "bench.out: $(cat "$root/out/only-run/bench.out" 2>/dev/null)"
    elif ! grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_ladder_refuses_a_symlink_ancestor_before_stash() {
    local name=ladder-symlink-ancestor root real module sys err dev mnt
    root=$(tmpdir)
    real=$root/real
    module=$real/loadtest
    sys=$root/sys
    err=$root/err
    seed_module "$module"
    ln -s "$real" "$root/alias"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    if MIN_FREE_GIB=0 with_fake_sys "$sys" "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$root/alias/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! state_in_place "$module"; then
        bad "$name" "index was stashed"
    elif ! grep -q "resolves under" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_a_symlink_ancestor() {
    local name=run-symlink-ancestor root err dev mnt
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/real/loadtest"
    ln -s "$root/real" "$root/alias"
    dev=$(mount_dev_for "$root/real/loadtest")
    mnt=$(mount_of "$root/real/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/alias/loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! grep -q "resolves under" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_refuses_a_recall_file_through_a_symlink_parent() {
    local name=run-recall-symlink-parent root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    mkdir -p "$root/safe/loadtest/real"
    ln -s "$root/safe/loadtest/real" "$root/safe/loadtest/link"
    printf 'entropy\n' >"$root/safe/loadtest/real/entropy-chunks.dat"
    stub_bench "$root"
    if with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --recall-file "$root/safe/loadtest/link/entropy-chunks.dat" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran"
    elif ! cmp -s <(printf 'entropy\n') "$root/safe/loadtest/real/entropy-chunks.dat"; then
        bad "$name" "recall bytes changed"
    elif ! grep -q "resolves under" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_a_recall_symlink_in_the_same_directory() {
    local name=run-recall-symlink-same-dir root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    printf 'entropy\n' >"$root/safe/loadtest/real-entropy.dat"
    ln -s real-entropy.dat "$root/safe/loadtest/entropy-chunks.dat"
    stub_bench "$root"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest" \
        --recall-file "$root/safe/loadtest/entropy-chunks.dat" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a symlink in the same directory: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif ! cmp -s <(printf 'entropy\n') "$root/safe/loadtest/real-entropy.dat"; then
        bad "$name" "recall bytes changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_run_allows_a_dotdot_that_stays_in_loadtest() {
    local name=run-dotdot-stays root err dev mnt
    root=$(tmpdir)
    err=$root/err
    loadtest_disk "$root" dev mnt
    stub_bench "$root"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device "$dev" --mount "$mnt" \
        --loadtest "$root/safe/loadtest/../loadtest" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh refused a collapsed .. that stays: $(cat "$err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# One --trace ladder. $2 is the Python source bound over blk-report.py.
# A sleeper stands in for blk-run.sh so the pause can finish without bpftrace.
# The ladder status is this function's status.
run_trace_ladder() {
    local root=$1
    local report_py=$2
    local module=$root/safe/loadtest
    local sys=$root/sys
    local err=$root/err
    local dev mnt
    seed_module "$module"
    dev=$(mount_dev_for "$module")
    mnt=$(mount_of "$module")
    mkdir -p "$sys/block/$dev/queue"
    queue_files "$sys/block/$dev/queue"
    stub_bench "$root"
    printf '%s\n' "$report_py" >"$root/blk-report.py"
    cat >"$root/blk-run.sh" <<'EOF'
#!/bin/bash
exec sleep 30
EOF
    chmod +x "$root/blk-run.sh"
    MIN_FREE_GIB=0 with_two_binds "$sys" \
        "$root/blk-run.sh" "$here/blk-run.sh" \
        "$root/blk-report.py" "$here/blk-report.py" \
        "$here/ladder.sh" \
        --device "$dev" --mount "$mnt" --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" \
        --trace --run only-run \
        >"$err" 2>&1
}

# A non-zero blk-report.py is a failed run. The done marker must not say success,
# and the index moved aside for the run comes back.
test_ladder_trace_exits_nonzero_when_blk_report_fails() {
    local name=ladder-trace-report-fails root
    root=$(tmpdir)
    if run_trace_ladder "$root" 'import sys; sys.exit(1)'; then
        bad "$name" "ladder.sh exited 0: $(cat "$root/err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run: $(cat "$root/err")"
    elif ! grep -q 'blk-report failed' "$root/out/only-run/blk-report.out"; then
        bad "$name" "report out: $(cat "$root/out/only-run/blk-report.out" 2>/dev/null) err=$(cat "$root/err")"
    elif ! state_in_place "$root/safe/loadtest"; then
        bad "$name" "index was not restored"
    elif grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker says success: $(cat "$root/out/ladder.done")"
    elif ! grep -q 'run only-run failed' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# A report that exits 0 still finishes the ladder. Exit 0 leaves the index stashed.
test_ladder_trace_exits_0_when_blk_report_succeeds() {
    local name=ladder-trace-report-ok root module
    root=$(tmpdir)
    module=$root/safe/loadtest
    if ! run_trace_ladder "$root" 'print("report ok")'; then
        bad "$name" "ladder.sh stopped: $(cat "$root/err")"
    elif [ ! -e "$root/bench-ran" ]; then
        bad "$name" "bench did not run"
    elif ! grep -qx 'report ok' "$root/out/only-run/blk-report.out"; then
        bad "$name" "report out: $(cat "$root/out/only-run/blk-report.out" 2>/dev/null)"
    elif grep -q 'blk-report failed' "$root/out/only-run/blk-report.out"; then
        bad "$name" "report was marked failed"
    elif ! cmp -s <(printf 'keep-db\n') "$module/state-prior/db"; then
        bad "$name" "prior db: $(cat "$module/state-prior/db" 2>/dev/null)"
    elif [ -e "$module/db" ]; then
        bad "$name" "exit 0 restored the index"
    elif ! cmp -s <(printf 'keep-chunks.dat\n') "$module/chunks.dat"; then
        bad "$name" "chunks.dat changed"
    elif ! grep -q '^restored max_sectors_kb=' "$root/out/ladder.done"; then
        bad "$name" "done marker: $(cat "$root/out/ladder.done" 2>/dev/null)"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

# Fenced sh examples that start the bench, or run.sh in front of it, pass
# --recall-direct-io. The flag table still says the bench default is off.
test_readme_bench_examples_pass_recall_direct_io() {
    local name=readme-recall-direct-io readme line
    local in_fence=0 fence="" saw=0
    readme=$here/../README.md
    if ! grep -F '| `--recall-direct-io` | off |' "$readme" >/dev/null; then
        bad "$name" "flag table no longer says the bench default is off"
        return
    fi
    while IFS= read -r line || [ -n "$line" ]; do
        if [ "$in_fence" -eq 0 ]; then
            if [ "$line" = '```sh' ]; then
                in_fence=1
                fence=""
            fi
            continue
        fi
        if [ "$line" = '```' ]; then
            in_fence=0
            if printf '%s\n' "$fence" | grep -F -e './target/release/irys-sm-bench' -e 'crates/tooling/sm-bench/scripts/run.sh' >/dev/null; then
                saw=$((saw + 1))
                if ! printf '%s\n' "$fence" | grep -F -e '--recall-direct-io' >/dev/null; then
                    bad "$name" "bench example omits --recall-direct-io ($(printf '%s\n' "$fence" | grep -m1 -v '^$'))"
                    return
                fi
            fi
            fence=""
            continue
        fi
        fence="${fence}
${line}"
    done <"$readme"
    if [ "$saw" -ne 5 ]; then
        bad "$name" "expected 5 bench examples, found $saw"
        return
    fi
    ok "$name"
}

# The built-in testnet consensus opens the mainnet partition file.
# 12.37 TiB is only the Custom scaffold in testnet_config.toml.
test_runbook_builtin_testnet_partition_is_the_mainnet_file() {
    local name=runbook-testnet-partition-size runbook
    runbook=$here/../../../../docs/60-indexes/06-storage-module-indexes.md
    if [ ! -f "$runbook" ]; then
        bad "$name" "missing $runbook"
        return
    fi
    if grep -F 'A testnet partition is 12.37 TiB and is under that cap.' "$runbook" >/dev/null; then
        bad "$name" "runbook says the built-in testnet partition fits ext4"
        return
    fi
    if ! grep -F 'opens that same 18.01 TiB file' "$runbook" >/dev/null; then
        bad "$name" "runbook does not say the built-in testnet file is 18.01 TiB"
        return
    fi
    if ! grep -F 'consensus = "Testnet"' "$runbook" >/dev/null; then
        bad "$name" "runbook does not name the built-in testnet consensus"
        return
    fi
    if ! grep -F 'testnet_config.toml' "$runbook" >/dev/null; then
        bad "$name" "runbook does not name the Custom scaffold file"
        return
    fi
    if ! grep -F 'num_chunks_in_partition = 51872000' "$runbook" >/dev/null; then
        bad "$name" "runbook does not name the scaffold chunk count"
        return
    fi
    if ! grep -F 'That limit applies to mainnet' "$runbook" >/dev/null; then
        bad "$name" "runbook does not keep the 18.01 TiB limit for both networks"
        return
    fi
    ok "$name"
}

# Same partition hash. Capacity that gains a ledger keeps bytes.
# Leaving a ledger clears the index.
test_runbook_same_hash_ledger_change_names_the_wipe() {
    local name=runbook-same-hash-ledger-wipe runbook
    runbook=$here/../../../../docs/60-indexes/06-storage-module-indexes.md
    local text
    if [ ! -f "$runbook" ]; then
        bad "$name" "missing $runbook"
        return
    fi
    text=$(tr '\n' ' ' <"$runbook")
    case "$text" in
        *'The same hash, ledger, and slot keeps the bytes.'*) ;;
        *) bad "$name" "runbook does not say an unchanged assignment keeps the bytes"; return ;;
    esac
    case "$text" in
        *'Capacity that gains a ledger keeps the bytes.'*) ;;
        *) bad "$name" "runbook does not say capacity that gains a ledger keeps the bytes"; return ;;
    esac
    case "$text" in
        *'Leaving a ledger (another ledger, another slot, or capacity)'*) ;;
        *) bad "$name" "runbook does not say leaving a ledger wipes"; return ;;
    esac
    case "$text" in
        *'rewrites the intervals to `Uninitialized`'* ) ;;
        *) bad "$name" "runbook does not say the wipe rewrites intervals to Uninitialized"; return ;;
    esac
    ok "$name"
}

main() {
    local fn
    if [ "$#" -gt 0 ]; then
        for fn in "$@"; do
            "$fn"
        done
    else
        while IFS= read -r fn; do
            "$fn"
        done < <(declare -F | awk '{ print $3 }' | grep '^test_')
    fi
    if [ "$fail" -ne 0 ]; then
        printf 'failed\n' >&2
        exit 1
    fi
    printf 'all ok\n'
}

main "$@"
