# Data Sync Test

Remote data synchronization test for validating partition data sync across a multi-node Irys cluster.

## Prerequisites

- Docker and Docker Compose installed and running
- Docker image built (see below)

## Building the Image

```bash
./build_image.sh
```

This builds the debug image (`irys:debug`) which includes a shell required for the test scripts.

With telemetry support (required for observability stack):
```bash
ENABLE_TELEMETRY=true ./build_image.sh
```

The image is tagged as `localhost/irys-test:latest` by default. Override with:
```bash
IMAGE_NAME=my-image:tag ./build_image.sh
```

## Running the Cluster

### Start the cluster
```bash
./start_cluster.sh
```

This starts all 3 Irys nodes and the observability stack, then waits for nodes to be ready.

Nodes are available at:
- <http://localhost:19080> (Node 1 - Genesis)
- <http://localhost:19081> (Node 2 - Peer)
- <http://localhost:19082> (Node 3 - Peer)

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

This script starts the cluster, runs the sync test, and cleans up on exit.

### Manual
```bash
docker compose down && docker compose up -d --force-recreate --remove-orphans && \
NODE_URLS="http://localhost:19080,http://localhost:19081,http://localhost:19082" \
cargo test --package irys-chain --test mod sync_partition_data_remote_test -- --nocapture --ignored
```

## Configuration

### Irys Nodes

- 3 Irys nodes on ports 19080, 19081, 19082
- Environment variables (can be overridden):
  - `IMAGE_NAME`: Docker image to use (default: `irys:debug`)
  - `MIN_HEIGHT`: Height when peer nodes start in the Docker compose (default: 40 for node 2, 50 for node 3)
  - `PEER_START_HEIGHT`: Height when rust test begins peer interaction (default: 60)

### Observability Stack

This test environment includes the shared observability stack from `docker/observation/`. See [Observation README](../../observation/README.md) for full details.

| Service | Port | Description |
|---------|------|-------------|
| Grafana | 3000 | Visualization dashboards (credentials: admin/admin) |
| Prometheus | 9090 | Metrics storage and querying |
| Tempo | 3200 | Distributed tracing backend |
| Elasticsearch | 9200 | Log storage with full-text search |
| OTEL Collector | 4317 (gRPC), 4318 (HTTP) | OpenTelemetry receiver |

**Accessing Grafana:**

```text
http://localhost:3000
Username: admin
Password: admin
```

**Telemetry Flow:**

```text
Irys Nodes → OTEL Collector → Tempo (traces)
                            → Prometheus (metrics)
                            → Elasticsearch (logs)
```

All Irys nodes are configured with `ENABLE_TELEMETRY=true` and export to the OTEL Collector.
