#!/usr/bin/env bash
# Run every formal verification suite. Each suite is formal/<name>/verify.sh.
# Does not install tools. About 10 minutes for the current suite when warm.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

shopt -s nullglob
found=("$root"/*/verify.sh)
if ((${#found[@]} == 0)); then
  printf 'no verify.sh under %s\n' "$root" >&2
  exit 1
fi

mapfile -t scripts < <(printf '%s\n' "${found[@]}" | LC_ALL=C sort)

for script in "${scripts[@]}"; do
  name=${script#"$root"/}
  name=${name%/verify.sh}
  printf '==> %s\n' "$name"
  if ! bash "$script"; then
    printf 'failed: %s\n' "$name" >&2
    exit 1
  fi
done

if ((${#scripts[@]} == 1)); then
  printf 'ok: 1 suite\n'
else
  printf 'ok: %d suites\n' "${#scripts[@]}"
fi
