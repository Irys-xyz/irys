#!/bin/bash
set -e

echo "=== PEER NODE STARTUP SCRIPT STARTED ==="
echo "Script: $0"
echo "Working directory: $(pwd)"
echo "User: $(whoami)"
echo "Date: $(date)"

# Configuration
GENESIS_NODE_URL="http://172.21.0.2:8080"
CONFIG_FILE="/app/config.toml"
TEMP_CONFIG="/app/config_with_genesis.toml"
MAX_RETRIES=30
RETRY_DELAY=5

echo "Starting peer node with dynamic genesis hash injection..."
echo "Genesis URL: $GENESIS_NODE_URL"
echo "Original config: $CONFIG_FILE"
echo "Modified config: $TEMP_CONFIG"

# Check if config file exists
if [ ! -f "$CONFIG_FILE" ]; then
    echo "ERROR: Config file not found at $CONFIG_FILE"
    ls -la /app/
    exit 1
fi

echo "Original config file found, size: $(wc -c < "$CONFIG_FILE") bytes"

# Wait for genesis node to be ready and fetch genesis hash
echo "Waiting for genesis node at $GENESIS_NODE_URL to be ready..."
for i in $(seq 1 $MAX_RETRIES); do
    if curl -s "$GENESIS_NODE_URL/v1/genesis" > /dev/null 2>&1; then
        echo "Genesis node is ready, fetching genesis hash..."
        GENESIS_RESPONSE=$(curl -s "$GENESIS_NODE_URL/v1/genesis")
        # Extract genesis_block_hash using grep and cut (no jq dependency)
        GENESIS_HASH=$(echo "$GENESIS_RESPONSE" | grep -o '"genesis_block_hash"[[:space:]]*:[[:space:]]*"[^"]*"' | cut -d'"' -f4)

        if [ "$GENESIS_HASH" != "null" ] && [ -n "$GENESIS_HASH" ]; then
            echo "Retrieved genesis hash: $GENESIS_HASH"
            break
        else
            echo "Genesis hash not yet available, retrying..."
        fi
    else
        echo "Genesis node not ready, retrying in ${RETRY_DELAY}s... (attempt $i/$MAX_RETRIES)"
    fi

    if [ $i -eq $MAX_RETRIES ]; then
        echo "ERROR: Failed to connect to genesis node after $MAX_RETRIES attempts"
        exit 1
    fi

    sleep $RETRY_DELAY
done

# Create modified config with the genesis hash
echo "Injecting genesis hash into config..."
cp "$CONFIG_FILE" "$TEMP_CONFIG"

# Check if expected_genesis_hash already exists in config
if grep -q "expected_genesis_hash" "$TEMP_CONFIG"; then
    # Replace existing genesis hash
    echo "Replacing existing genesis hash..."
    sed -i "s/expected_genesis_hash = .*/expected_genesis_hash = \"$GENESIS_HASH\"/" "$TEMP_CONFIG"
else
    # Add genesis hash right after [consensus.Custom] line
    echo "Adding genesis hash to config..."
    awk -v genesis="expected_genesis_hash = \"$GENESIS_HASH\"" '
        /^\[consensus\.Custom\]$/ { print; print genesis; next }
        { print }
    ' "$TEMP_CONFIG" > "${TEMP_CONFIG}.new" && mv "${TEMP_CONFIG}.new" "$TEMP_CONFIG"
fi

# Verify the replacement worked
if grep -q "expected_genesis_hash = \"$GENESIS_HASH\"" "$TEMP_CONFIG"; then
    echo "SUCCESS: Genesis hash successfully injected into config"
else
    echo "ERROR: Failed to inject genesis hash into config"
    echo "Current genesis hash line in config:"
    grep "expected_genesis_hash" "$TEMP_CONFIG" || echo "No genesis hash line found!"
    exit 1
fi

echo "Config updated with genesis hash: $GENESIS_HASH"

# Verify the genesis hash was injected
echo "Verifying genesis hash injection..."
grep "expected_genesis_hash" "$TEMP_CONFIG" || echo "WARNING: Genesis hash not found in config!"

# Start the node with the updated config
echo "Starting Irys node with config: $TEMP_CONFIG"
export CONFIG="$TEMP_CONFIG"
exec /app/irys
