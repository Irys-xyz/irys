# Docker Setup

Docker configuration for running Irys nodes.

## Images

| Image | Dockerfile | Description |
|-------|------------|-------------|
| `irys:release` | `Dockerfile.release` | Production distroless image (no shell) |
| `irys:debug` | `Dockerfile.debug` | Debug image with shell for testing |

## Release Cluster

A 2-node cluster using the production distroless image.

### Build the release image

```bash
./build_release_image.sh
```

### Run the cluster

```bash
./run_release_cluster.sh
```

This starts:
- **irys-1** (Genesis node): http://localhost:19080
- **irys-2** (Peer node): http://localhost:19081

### View logs

```bash
docker compose -f docker-compose.release.yaml logs -f
```

### Stop the cluster

```bash
docker compose -f docker-compose.release.yaml down -v
```

## Test Cluster

A 3-node cluster for data sync testing. Uses the debug image (has shell for test scripts).

See `tests/data-sync/` for details.

```bash
cd tests/data-sync
./build_image.sh    # Build debug image
./start_cluster.sh  # Start 3-node cluster
```

