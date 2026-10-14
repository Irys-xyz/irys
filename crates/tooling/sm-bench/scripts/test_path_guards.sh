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
