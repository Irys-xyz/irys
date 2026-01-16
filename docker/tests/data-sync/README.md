# Data Sync Test

Remote data synchronization test for validating partition data sync across a multi-node Irys cluster.

## Prerequisites

- Docker and Docker Compose installed and running
- Docker image built (see below)

## Building the Docker Image

```bash
./build_image.sh
```

This builds the debug image (`irys:debug`) which includes a shell required for the test scripts.

## Running the Test Cluster

### Start the cluster

```bash
./start_cluster.sh
```

This starts a 3-node cluster:
- **test-irys-1** (Genesis node): <http://localhost:19080>
- **test-irys-2** (Peer node): <http://localhost:19081>
- **test-irys-3** (Peer node): <http://localhost:19082>

### View logs

```bash
docker compose logs -f
```

### Stop the cluster

```bash
docker compose down -v
```

## Running the Sync Test

### Automated (Recommended)

```bash
./run_test.sh
```

This script automatically manages containers and runs the test.

### Manual

```bash
docker compose down && docker compose up -d --force-recreate --remove-orphans && \
NODE_URLS="http://localhost:19080,http://localhost:19081,http://localhost:19082" \
cargo test --package irys-chain --test mod sync_partition_data_remote_test -- --nocapture --ignored
```

## Configuration

- 3 Irys nodes on ports 19080, 19081, 19082
- Environment variables (can be overridden):
  - `IMAGE_NAME`: Docker image to use (default: `irys:debug`)
  - `MIN_HEIGHT`: Height when peer nodes start (default: 40 for node 2, 50 for node 3)
  - `PEER_START_HEIGHT`: Height when rust test begins peer interaction (default: 60)
