#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
REPO_ROOT="$SCRIPT_DIR/../../.."
cd "$SCRIPT_DIR"

# Default to debug image (has shell for test scripts)
IMAGE_NAME="${IMAGE_NAME:-irys:debug}"
export IMAGE_NAME

# Function to cleanup on exit
cleanup() {
    echo ""
    echo "Shutting down containers..."
    docker compose down -v 2>/dev/null || true
}

# Set up trap to cleanup on exit
trap cleanup EXIT

echo "========================================"
echo "Irys Remote Partition Sync Test"
echo "========================================"
echo "Using image: $IMAGE_NAME"
echo ""

# Check if image exists
if ! docker image inspect "$IMAGE_NAME" > /dev/null 2>&1; then
    echo "Image '$IMAGE_NAME' not found."
    echo "Run ./build_image.sh first to build the image."
    exit 1
fi

# Check if containers are already running
echo "Checking if containers are running..."
containers_running=true
for container in test-irys-1 test-irys-2 test-irys-3; do
    if ! docker ps --format '{{.Names}}' | grep -q "^${container}$"; then
        containers_running=false
        break
    fi
done

if [ "$containers_running" = false ]; then
    echo "Containers not running. Starting..."
    docker compose up -d

    # Wait for nodes to be ready
    echo "Waiting for nodes to initialize..."
    for port in 19080 19081 19082; do
        echo -n "  Checking localhost:$port... "
        max_attempts=30
        attempt=0

        while [ $attempt -lt $max_attempts ]; do
            if curl -s http://localhost:$port/v1/info > /dev/null 2>&1; then
                echo "✓ Ready"
                break
            fi
            attempt=$((attempt + 1))
            if [ $attempt -eq $max_attempts ]; then
                echo "✗ Failed"
                echo "ERROR: Node on port $port not responding after 30 seconds"
                exit 1
            fi
            sleep 5
        done
    done
else
    echo "Containers already running."
fi

# Run the sync_partition_data_remote_test
echo ""
echo "Running sync_partition_data_remote_test..."
echo "========================================"

NODE_URLS="http://localhost:19080,http://localhost:19081,http://localhost:19082" \
cargo test --package irys-chain --test mod sync_partition_data_remote_test -- --nocapture --ignored

test_result=$?

echo ""
echo "========================================"
if [ $test_result -eq 0 ]; then
    echo "✓ Test passed"
else
    echo "✗ Test failed"
fi

exit $test_result
