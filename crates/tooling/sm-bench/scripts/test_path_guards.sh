#!/bin/bash
# Regression checks for the sm-bench shell guards.
# Throwaway directories under /tmp only. Does not write real devices or sysfs.
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

test_fill_allows_a_loadtest_file_and_stops_before_write() {
    local name=fill-allow root file
    root=$(tmpdir)
    file=$root/loadtest/live.dat
    mkdir -p "$root/loadtest"
    printf 'LIVE-NODE-BYTES' >"$file"
    touch "$root/STOP"
    if ! "$here/fill.sh" --file "$file" --log "$root/fill.log" --stop-file "$root/STOP" --segment-bytes 16 >"$root/err" 2>&1; then
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

test_run_allows_a_missing_path_under_loadtest() {
    local name=run-allow-missing root err
    root=$(tmpdir)
    err=$root/err
    mkdir -p "$root/safe/loadtest" "$root/sys/block/none/queue"
    queue_files "$root/sys/block/none/queue"
    printf '#!/bin/sh\necho ran >"%s/bench-ran"\nexit 0\n' "$root" >"$root/bench"
    chmod +x "$root/bench"
    if ! with_fake_sys "$root/sys" "$here/run.sh" \
        --name t --device none --mount /tmp \
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
