#!/usr/bin/env bash
set -Eeuo pipefail
IFS=$'\n\t'

# =========================
# Config (overridable via env)
# =========================
GENESIS_URL="${GENESIS_URL:-http://172.21.0.2:8080}"
LOCAL_URL="${LOCAL_URL:-http://localhost:8080}"
INFO_PATH="${INFO_PATH:-/v1/info}"
GENESIS_PATH="${GENESIS_PATH:-/v1/genesis}"
LEDGER_PATH="${LEDGER_PATH:-/ledger}"

MIN_HEIGHT="${MIN_HEIGHT:-40}"

CHECK_INTERVAL_SEC="${CHECK_INTERVAL_SEC:-10}"      # monitor poll interval
STUCK_THRESHOLD="${STUCK_THRESHOLD:-3}"             # consecutive equal heights before restart
CURL_TIMEOUT="${CURL_TIMEOUT:-5}"                   # seconds
RESTART_BACKOFF_BASE="${RESTART_BACKOFF_BASE:-2}"   # exponential backoff base
RESTART_BACKOFF_MAX_SEC="${RESTART_BACKOFF_MAX_SEC:-60}"

IRYS_BIN="${IRYS_BIN:-/app/irys}"
BASE_CONFIG="${BASE_CONFIG:-/app/config.toml}"

# Will write a temp config if we learn the genesis hash.
TMP_CONFIG=""
NODE_PID=""
MONITOR_PID=""
RESTARTS=0

log() { printf '[%(%Y-%m-%dT%H:%M:%S%z)T] %s\n' -1 "$*"; }

have_jq() { command -v jq >/dev/null 2>&1; }

curl_json() {
  # $1 = URL
  curl -sS --fail --max-time "$CURL_TIMEOUT" --connect-timeout "$CURL_TIMEOUT" "$1" 2>/dev/null || echo "{}"
}

extract_height() {
  # input: JSON string
  if have_jq; then
    # handle height stored as string or number
    jq -r '(.height // .height? // "0") | tostring | capture("(?<n>\\d+)")?.n // "0"' 2>/dev/null || echo "0"
  else
    # fallback to grep/sed
    grep -oP '"height"\s*:\s*"\K[0-9]+' 2>/dev/null || \
    sed -n 's/.*"height"[[:space:]]*:[[:space:]]*"\([0-9]\+\)".*/\1/p' 2>/dev/null || \
    echo "0"
  fi
}

get_height() {
  # $1 = BASE URL (e.g., http://host:8080)
  local resp height
  resp="$(curl_json "$1$INFO_PATH")"
  height="$(printf '%s' "$resp" | extract_height)"
  [[ "$height" =~ ^[0-9]+$ ]] || height="0"
  printf '%s' "$height"
}

get_genesis_hash() {
  # Try /v1/genesis first, then /ledger
  local resp hash

  resp="$(curl_json "$GENESIS_URL$GENESIS_PATH")"
  if have_jq; then
    hash="$(printf '%s' "$resp" | jq -r '.genesis_block_hash // empty' 2>/dev/null || true)"
  else
    hash="$(printf '%s' "$resp" | grep -o '"genesis_block_hash"[[:space:]]*:[[:space:]]*"[^"]*"' | cut -d'"' -f4 || true)"
  fi

  if [[ -z "${hash:-}" || "$hash" == "null" ]]; then
    resp="$(curl_json "$GENESIS_URL$LEDGER_PATH")"
    if have_jq; then
      hash="$(printf '%s' "$resp" | jq -r '.genesis_hash // empty' 2>/dev/null || true)"
    else
      hash="$(printf '%s' "$resp" | grep -oP '"genesis_hash":"\K[^"]+' || true)"
    fi
  fi

  printf '%s' "${hash:-}"
}

make_config_with_genesis() {
  local ghash="$1"
  TMP_CONFIG="$(mktemp -t irys-config.XXXXXX.toml)"
  cp "$BASE_CONFIG" "$TMP_CONFIG"
  # Replace empty expected_genesis_hash = "" with the discovered hash.
  sed -i "s/expected_genesis_hash = \"\"/expected_genesis_hash = \"$ghash\"/g" "$TMP_CONFIG"
  printf '%s' "$TMP_CONFIG"
}

start_node() {
  log "Starting Irys peer node..."
  log "Waiting for genesis node to reach height $MIN_HEIGHT..."

  for i in $(seq 1 30); do
    local h
    h="$(get_height "$GENESIS_URL")"
    if [[ "$h" -ge "$MIN_HEIGHT" ]]; then
      log "Genesis node at height $h, proceeding..."
      break
    fi
    log "Genesis node height: $h, waiting for $MIN_HEIGHT... (attempt $i/30)"
    sleep "$CHECK_INTERVAL_SEC"
  done

  local ghash
  ghash="$(get_genesis_hash || true)"
  if [[ -n "${ghash:-}" && "$ghash" != "null" ]]; then
    log "Got genesis hash: $ghash"
    local cfg
    cfg="$(make_config_with_genesis "$ghash")"
    export CONFIG="$cfg"
    log "Config updated with genesis hash; using: $CONFIG"
  else
    export CONFIG="$BASE_CONFIG"
    log "Failed to get genesis hash, starting with base config: $CONFIG"
  fi

  # Launch node in foreground (so we can supervise PID)
  "$IRYS_BIN"
}

monitor_and_restart() {
  local stuck_count=0
  local last_height=0

  while true; do
    sleep "$CHECK_INTERVAL_SEC"

    local height
    height="$(get_height "$LOCAL_URL")"
    [[ "$height" =~ ^[0-9]+$ ]] || height="0"

    log "Current height: $height (last: $last_height)"

    # Has node made progress?
    if [[ "$height" -gt "$last_height" ]]; then
      stuck_count=0
      last_height="$height"
      continue
    fi

    # "Stuck" only if >0 and equal
    if [[ "$height" -gt 0 && "$height" -eq "$last_height" ]]; then
      ((stuck_count++))
      log "Stuck at height $height, count: $stuck_count"

      if [[ "$stuck_count" -ge "$STUCK_THRESHOLD" ]]; then
        log "Node stuck at height $height for too long, restarting..."
        restart_node
        # reset for the next cycle
        stuck_count=0
        last_height=0
      fi
    fi
  done
}

restart_node() {
  # Backoff grows with each restart up to a cap
  ((RESTARTS++))
  local backoff=$(( RESTART_BACKOFF_BASE ** (RESTARTS-1) ))
  (( backoff > RESTART_BACKOFF_MAX_SEC )) && backoff="$RESTART_BACKOFF_MAX_SEC"

  if [[ -n "${NODE_PID:-}" ]]; then
    kill -TERM "$NODE_PID" 2>/dev/null || true
    # wait up to 10s then SIGKILL
    for _ in {1..20}; do
      if ! kill -0 "$NODE_PID" 2>/dev/null; then break; fi
      sleep 0.5
    done
    if kill -0 "$NODE_PID" 2>/dev/null; then
      log "Node not exiting, sending SIGKILL"
      kill -KILL "$NODE_PID" 2>/dev/null || true
    fi
  fi

  # Clean any temp config from previous run
  if [[ -n "${TMP_CONFIG:-}" && -f "$TMP_CONFIG" ]]; then
    rm -f "$TMP_CONFIG" || true
    TMP_CONFIG=""
  fi

  log "Backoff before restart: ${backoff}s"
  sleep "$backoff"

  # Re-launch node as child of a subshell to capture PID
  ( start_node ) &
  NODE_PID=$!
  log "Node restarted with PID $NODE_PID"
}

shutdown_all() {
  log "Caught signal, shutting down..."
  if [[ -n "${MONITOR_PID:-}" ]]; then
    kill "$MONITOR_PID" 2>/dev/null || true
  fi
  if [[ -n "${NODE_PID:-}" ]]; then
    kill -TERM "$NODE_PID" 2>/dev/null || true
    wait "$NODE_PID" 2>/dev/null || true
  fi
  if [[ -n "${TMP_CONFIG:-}" && -f "$TMP_CONFIG" ]]; then
    rm -f "$TMP_CONFIG" || true
  fi
  exit 0
}

trap shutdown_all INT TERM

# =========================
# Main
# =========================
log "Starting peer with auto-restart capability..."

( start_node ) &
NODE_PID=$!
log "Node started with PID $NODE_PID"

# Start monitor
( monitor_and_restart ) &
MONITOR_PID=$!
log "Monitor started with PID $MONITOR_PID"

# Wait on the monitor; let it handle all node restarts
# The monitor will only exit on signals or critical errors
set +e
wait "$MONITOR_PID"
MONITOR_STATUS=$?
set -e
log "Monitor exited with status $MONITOR_STATUS"
# Clean up any remaining node process
if [[ -n "${NODE_PID:-}" ]]; then
  kill -TERM "$NODE_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
fi
exit "$MONITOR_STATUS"
