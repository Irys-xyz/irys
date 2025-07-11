#!/bin/bash

# Clean up
rm -rf .irys

# Build first
echo "Building irys..."
cargo build --bin irys

# Run the node in the background
echo "Starting node..."
GENESIS=TRUE RUST_LOG=debug cargo run --bin irys &
NODE_PID=$!

echo "Started node with PID: $NODE_PID"

# Wait 20 seconds then send SIGINT (Ctrl-C)
sleep 20
echo "Sending SIGINT to node..."
kill -INT $NODE_PID

# Wait 10 more seconds then force kill if still running
sleep 10
if ps -p $NODE_PID > /dev/null; then
    echo "Node still running, sending SIGKILL..."
    kill -KILL $NODE_PID
fi

echo "Node terminated"