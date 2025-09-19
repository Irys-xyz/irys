#!/usr/bin/env sh
set -eu

# Make sure binary is executable (when copied it should already be)
chmod +x /app/irys || true

# Configure ufw rules if available; not all container runtimes enable it
if command -v ufw >/dev/null 2>&1; then
  ufw default deny incoming || true
  ufw allow 8080 || true
  ufw allow 9009 || true
  ufw allow 9010 || true
  ufw --force enable || true
fi

# If no arguments provided, use default config
if [ $# -eq 0 ]; then
  exec /app/irys --config /app/config.toml
else
  exec /app/irys "$@"
fi
