#!/usr/bin/env sh
set -eu

if [ $# -eq 0 ]; then
  exec /app/irys --config /app/config.toml
else
  exec /app/irys "$@"
fi
