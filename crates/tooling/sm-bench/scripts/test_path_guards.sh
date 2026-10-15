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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"

    if with_fake_sys "$sys" "$here/run.sh" \
        --name t --device dm-0 --mount /tmp \
        --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0 for dm-0: $(cat "$err")"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "run.sh rejected dm-0: $(cat "$err")"
    elif ! grep -q "not dm-0" "$err"; then
        bad "$name" "run.sh did not reach the mount check for dm-0: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran for dm-0"
    else
        ok "$name-run"
    fi

    if with_fake_sys "$sys" "$here/run.sh" \
        --name t --device data-root --mount /tmp \
        --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh exited 0 for data-root: $(cat "$err")"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "run.sh rejected data-root: $(cat "$err")"
    elif ! grep -q "not data-root" "$err"; then
        bad "$name" "run.sh did not reach the mount check for data-root: $(cat "$err")"
    else
        ok "$name-run-mapper"
    fi

    if with_fake_sys "$sys" "$here/run.sh" \
        --name t --device .. --mount /tmp \
        --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --min-free-gib 0 \
        >"$err" 2>&1; then
        bad "$name" "run.sh accepted .."
    elif ! grep -q "alphanumeric" "$err"; then
        bad "$name" "run.sh did not reject ..: $(cat "$err")"
    elif [ -e "$root/bench-ran" ]; then
        bad "$name" "bench ran for .."
    else
        ok "$name-run-dotdot"
    fi

    if with_fake_sys "$sys" "$here/ladder.sh" \
        --device dm-0 --mount /tmp --loadtest "$module" \
        --out-root "$root/out" --bench "$root/bench" --run only-run \
        >"$err" 2>&1; then
        bad "$name" "ladder.sh exited 0 for dm-0: $(cat "$err")"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "ladder.sh rejected dm-0: $(cat "$err")"
    elif ! grep -q "not dm-0" "$err"; then
        bad "$name" "ladder.sh did not reach the mount check: $(cat "$err")"
    else
        ok "$name-ladder"
    fi

    printf '999\n' >"$queue/max_sectors_kb"
    cat >"$root/out/sysfs.txt" <<'EOF'
max_sectors_kb=111
read_ahead_kb=128
nr_requests=64
scheduler=[mq-deadline]
EOF
    # A finished pid is enough. The charset case is not a live bench.
    sleep 0.1 &
    dead=$!
    wait "$dead" 2>/dev/null || true
    printf '%s\n' "$dead" >"$root/out/run.pid"
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
    if with_fake_sys "$sys" "$here/teardown.sh" . "$root/out/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown.sh accepted ."
    elif ! grep -q "alphanumeric" "$err"; then
        bad "$name" "teardown.sh did not reject .: $(cat "$err")"
    elif ! grep -q '^999$' "$queue/max_sectors_kb"; then
        bad "$name" "teardown.sh wrote the queue for ."
    else
        ok "$name-teardown-dot"
    fi

    if with_fake_sys "$sys" "$here/teardown.sh" "" "$root/out/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown.sh accepted an empty name"
    elif ! grep -q "alphanumeric" "$err"; then
        bad "$name" "teardown.sh did not reject an empty name: $(cat "$err")"
    else
        ok "$name-teardown-empty"
    fi

    if "$here/sample.sh" dm-0 "$root/out" >"$err" 2>&1; then
        bad "$name" "sample.sh exited 0"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "sample.sh rejected dm-0: $(cat "$err")"
    elif ! grep -q "iostat" "$err"; then
        bad "$name" "sample.sh did not reach iostat: $(cat "$err")"
    else
        ok "$name-sample"
    fi

    if "$here/sample.sh" "sda/sdb" "$root/out" >"$err" 2>&1; then
        bad "$name" "sample.sh accepted a slash"
    elif ! grep -q "alphanumeric" "$err"; then
        bad "$name" "sample.sh did not reject a slash: $(cat "$err")"
    else
        ok "$name-sample-slash"
    fi

    if "$here/blk-run.sh" dm-0 "$module" "$root/trace" >"$err" 2>&1; then
        bad "$name" "blk-run.sh exited 0 for dm-0: $(cat "$err")"
    elif grep -q "alphanumeric" "$err"; then
        bad "$name" "blk-run.sh rejected dm-0: $(cat "$err")"
    elif ! grep -q "not dm-0" "$err"; then
        bad "$name" "blk-run.sh did not reach the mount check: $(cat "$err")"
    else
        ok "$name-blk-run"
    fi

    if "$here/blk-run.sh" .. "$module" "$root/trace" >"$err" 2>&1; then
        bad "$name" "blk-run.sh accepted .."
    elif ! grep -q "alphanumeric" "$err"; then
        bad "$name" "blk-run.sh did not reject ..: $(cat "$err")"
    else
        ok "$name-blk-run-dotdot"
    fi

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

test_teardown_does_not_restore_when_the_pid_is_not_numeric() {
    local name=teardown-bad-pid root err
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    printf 'not-a-pid\n' >"$root/run/run.pid"
    if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown exited 0 for a non-numeric pid"
    elif ! grep -q "not a pid" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! grep -q '^999$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue changed: $(cat "$root/sys/block/notadisk/queue/max_sectors_kb")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_teardown_does_not_restore_when_the_pid_file_is_missing() {
    local name=teardown-missing-pid root err
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown exited 0 with no run.pid"
    elif ! grep -q "missing" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! grep -q '^999$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue changed: $(cat "$root/sys/block/notadisk/queue/max_sectors_kb")"
    else
        ok "$name"
    fi
    rm -rf "$root"
}

test_teardown_does_not_restore_when_the_pid_file_is_empty() {
    local name=teardown-empty-pid root err
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    : >"$root/run/run.pid"
    if with_fake_sys "$root/sys" "$here/teardown.sh" notadisk "$root/run/sysfs.txt" >"$err" 2>&1; then
        bad "$name" "teardown exited 0 for an empty run.pid"
    elif ! grep -q "not a pid" "$err"; then
        bad "$name" "stderr: $(cat "$err")"
    elif ! grep -q '^999$' "$root/sys/block/notadisk/queue/max_sectors_kb"; then
        bad "$name" "queue changed"
    else
        ok "$name"
    fi
    rm -rf "$root"
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
    local name=teardown-dead-pid root err dead
    root=$(tmpdir)
    err=$root/err
    queue_restore_fixture "$root"
    sleep 30 &
    dead=$!
    kill "$dead" 2>/dev/null || true
    wait "$dead" 2>/dev/null || true
    printf '%s\n' "$dead" >"$root/run/run.pid"
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
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest" "$root/bin"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    cat >"$root/bin/df" <<EOF
#!/bin/bash
printf '%s\\n' "\$*" >> "$root/df.args"
exec /usr/bin/df "\$@"
EOF
    chmod +x "$root/bin/df"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    install_rounding_df "$root"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
    install_rounding_df "$root"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
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
    mkdir -p "$root/safe/loadtest"
    dev=$(mount_dev_for "$root/safe/loadtest")
    mnt=$(mount_of "$root/safe/loadtest")
    mkdir -p "$root/sys/block/$dev/queue"
    queue_files "$root/sys/block/$dev/queue"
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
    # bpftrace exits on SIGINT. GNU sleep ignores SIGINT, so the stub cannot be sleep.
    cat >"$root/bin/bpftrace" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bpf.pid"
exec python3 -c 'import signal,sys,time
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
time.sleep(120)'
EOF
    cat >"$root/bin/sleep" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >> "$root/sleep.log"
exec /bin/sleep "\$@"
EOF
    cat >"$root/extent-snap.sh" <<'EOF'
#!/bin/bash
exit 0
EOF
    chmod +x "$root/bin/bpftrace" "$root/bin/sleep" "$root/extent-snap.sh"
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
    # bpftrace exits on SIGINT. GNU sleep ignores SIGINT, so the stub cannot be sleep.
    cat >"$root/bin/bpftrace" <<EOF
#!/bin/bash
printf '%s\n' "\$\$" > "$root/bpf.pid"
exec python3 -c 'import signal,sys,time
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
time.sleep(120)'
EOF
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
    chmod +x "$root/bin/bpftrace" "$root/extent-snap.sh"
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
