#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"

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
    echo "Containers not running. Starting Docker Compose..."
    # Only build if --build flag is passed or image doesn't exist
    BUILD_FLAG=""
    if [[ " $* " == *" --build "* ]] || ! docker images --format '{{.Repository}}:{{.Tag}}' | grep -q "^irys-test:latest$"; then
        BUILD_FLAG="--build"
        echo "Building image (use existing with --no-build to skip)..."
    fi
    DOCKER_BUILDKIT=0 docker compose up -d $BUILD_FLAG

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
