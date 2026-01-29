#!/usr/bin/env sh
set -eu

# Create data directory if it doesn't exist
mkdir -p /app/data

# Copy staged submodules config into data directory if present
if [ -f /app/staged/.irys_submodules.toml ]; then
  cp /app/staged/.irys_submodules.toml /app/data/.irys_submodules.toml
fi

if [ $# -eq 0 ]; then
  exec /app/irys --config /app/config.toml
else
  exec /app/irys "$@"
fi
