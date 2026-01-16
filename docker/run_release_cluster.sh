#!/usr/bin/env bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"

IMAGE_NAME="${IMAGE_NAME:-irys:release}"
export IMAGE_NAME

echo "========================================"
echo "Irys Release Cluster"
echo "========================================"
echo "Using image: $IMAGE_NAME"
echo ""

# Check if image exists
if ! docker image inspect "$IMAGE_NAME" > /dev/null 2>&1; then
    echo "Image '$IMAGE_NAME' not found."
    echo "Run ./build_release_image.sh first to build the image."
    exit 1
fi

# Check if containers are already running
echo "Checking if containers are running..."
containers_running=true
for container in irys-1 irys-2; do
    if ! docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null | grep -q "true"; then
        containers_running=false
        break
    fi
done

if [ "$containers_running" = false ]; then
    echo "Starting containers..."
    docker compose -f docker-compose.release.yaml up -d

    # Wait for nodes to be ready
    echo "Waiting for nodes to initialize..."
    for port in 19080 19081; do
        echo -n "  Checking localhost:$port... "
        max_attempts=30
        attempt=0

        while [ $attempt -lt $max_attempts ]; do
            if curl -s http://localhost:$port/v1/info > /dev/null 2>&1; then
                echo "Ready"
                break
            fi
            attempt=$((attempt + 1))
            if [ $attempt -eq $max_attempts ]; then
                echo "Failed"
                echo "ERROR: Node on port $port not responding after 30 seconds"
                echo ""
                echo "Check logs with: docker compose -f docker-compose.release.yaml logs"
                exit 1
            fi
            sleep 1
        done
    done
else
    echo "Containers already running."
fi

echo ""
echo "========================================"
echo "Cluster is running"
echo ""
echo "Nodes:"
echo "  - http://localhost:19080"
echo "  - http://localhost:19081"
echo ""
echo "Logs:  docker compose -f $SCRIPT_DIR/docker-compose.release.yaml logs -f"
echo "Stop:  docker compose -f $SCRIPT_DIR/docker-compose.release.yaml down -v"
echo "========================================"
