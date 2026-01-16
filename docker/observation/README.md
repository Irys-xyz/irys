# Observability Stack

Reusable monitoring infrastructure for Irys development and testing environments.

## Components

| Service | Port | Description |
|---------|------|-------------|
| Grafana | 3000 | Visualization dashboards |
| Prometheus | 9090 | Metrics storage and querying |
| Tempo | 3200 | Distributed tracing backend |
| Elasticsearch | 9200 | Log storage with full-text search |
| OTEL Collector | 4317 (gRPC), 4318 (HTTP) | OpenTelemetry receiver |

## Quick Start

### Standalone
```bash
docker compose up -d
```

### With Custom Ports
```bash
GRAFANA_PORT=3001 PROMETHEUS_PORT=9091 docker compose up -d
```

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `GRAFANA_PORT` | 3000 | Grafana HTTP port |
| `GRAFANA_ADMIN_USER` | admin | Grafana admin username |
| `GRAFANA_ADMIN_PASSWORD` | admin | Grafana admin password |
| `PROMETHEUS_PORT` | 9090 | Prometheus HTTP port |
| `PROMETHEUS_RETENTION` | 7d | Prometheus data retention |
| `TEMPO_PORT` | 3200 | Tempo HTTP port |
| `ELASTICSEARCH_PORT` | 9200 | Elasticsearch HTTP port |
| `OTEL_GRPC_PORT` | 4317 | OTEL Collector gRPC port |
| `OTEL_HTTP_PORT` | 4318 | OTEL Collector HTTP port |
| `LOG_RETENTION_DAYS` | 7 | Elasticsearch log retention (days) |
| `ES_JAVA_OPTS` | -Xms2g -Xmx2g | Elasticsearch JVM options |
| `ES_CLUSTER_NAME` | observation-cluster | Elasticsearch cluster name |

## Integration

### Including in Other Compose Files

Use Docker Compose's `include` directive to add the observability stack to your project:

```yaml
# docker/tests/your-test/docker-compose.yaml
include:
  - path: ../../observation/docker-compose.yaml

services:
  irys-node:
    image: localhost/irys-test:latest
    environment:
      - ENABLE_TELEMETRY=true
      - OTEL_SERVICE_NAME=irys-node-1
      - OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
      - OTEL_LOGS_ENDPOINT=http://otel-collector:4318/v1/logs
    networks:
      - observation_network
    depends_on:
      otel-collector:
        condition: service_healthy
```

### Connecting Irys Nodes

Configure Irys nodes to send telemetry to the OTEL Collector:

```yaml
environment:
  - RUST_LOG=debug
  - ENABLE_TELEMETRY=true
  - OTEL_SERVICE_NAME=irys-node-1    # Unique name per node
  - OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
  - OTEL_LOGS_ENDPOINT=http://otel-collector:4318/v1/logs
```

Each node should have a unique `OTEL_SERVICE_NAME` (e.g., `irys-node-1`, `irys-node-2`) for filtering in Grafana dashboards.

## Architecture

```text
┌─────────────────┐
│  Applications   │
│  (Irys nodes)   │
└────────┬────────┘
         │ OTLP (gRPC/HTTP)
         ▼
┌─────────────────┐
│  OTEL Collector │
└────────┬────────┘
         │
    ┌────┼────┬────────────┐
    │    │    │            │
    ▼    ▼    ▼            ▼
┌──────┐┌──────────┐┌─────────────┐
│Tempo ││Prometheus││Elasticsearch│
│traces││ metrics  ││    logs     │
└──────┘└──────────┘└─────────────┘
    │         │            │
    └─────────┴─────┬──────┘
                    ▼
              ┌─────────┐
              │ Grafana │
              └─────────┘
```

## Accessing Services

### Grafana

```text
URL: http://localhost:3000
Username: admin
Password: admin
```

Pre-configured datasources:
- **Elasticsearch** (default): Log search and analysis
- **Prometheus**: Metrics queries
- **Tempo**: Trace exploration

### Direct Access

- **Prometheus**: <http://localhost:9090>
- **Tempo**: <http://localhost:3200>
- **Elasticsearch**: <http://localhost:9200>

## Data Retention

- **Prometheus metrics**: 7 days (configurable via `PROMETHEUS_RETENTION`)
- **Elasticsearch logs**: 7 days (configurable via `LOG_RETENTION_DAYS`)
- **Tempo traces**: 7 days (168h)

## Cleanup

Remove all containers and data:
```bash
docker compose down -v
```
