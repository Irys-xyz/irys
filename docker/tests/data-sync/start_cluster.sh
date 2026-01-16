#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"

# Default to debug image (has shell for test scripts)
IMAGE_NAME="${IMAGE_NAME:-irys:debug}"
export IMAGE_NAME

echo "========================================"
echo "Irys Test Cluster"
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
    echo "Starting containers..."
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

echo ""
echo "========================================"
echo "✓ Cluster is running"
echo ""
echo "Nodes:"
echo "  - http://localhost:19080"
echo "  - http://localhost:19081"
echo "  - http://localhost:19082"
echo ""
echo "To stop: docker compose down -v"
echo "========================================"
