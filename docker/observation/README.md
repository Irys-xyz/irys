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

Copy `.env.example` to `.env` and customize as needed:
```bash
cp .env.example .env
# Edit .env with your configuration
```

| Variable | Default | Description |
|----------|---------|-------------|
| `GRAFANA_PORT` | 3000 | Grafana HTTP port |
| `GRAFANA_BIND_HOST` | 127.0.0.1 | Grafana bind address (use 0.0.0.0 to expose externally) |
| `GRAFANA_ANON_ENABLED` | false | Enable anonymous viewer access |
| `GRAFANA_ADMIN_USER` | admin | Grafana admin username |
| `GRAFANA_ADMIN_PASSWORD` | admin | Grafana admin password ⚠️ Change in production! |
| `PROMETHEUS_PORT` | 9090 | Prometheus HTTP port |
| `PROMETHEUS_RETENTION` | 7d | Prometheus data retention |
| `TEMPO_PORT` | 3200 | Tempo HTTP port |
| `ELASTICSEARCH_PORT` | 9200 | Elasticsearch HTTP port |
| `OTEL_GRPC_PORT` | 4317 | OTEL Collector gRPC port |
| `OTEL_HTTP_PORT` | 4318 | OTEL Collector HTTP port |
| `LOG_RETENTION_DAYS` | 7 | Elasticsearch log retention (days) |
| `ES_JAVA_OPTS` | -Xms512m -Xmx512m | Elasticsearch JVM options (increase for production) |
| `ES_CLUSTER_NAME` | observation-cluster | Elasticsearch cluster name |

## Integration

### Including in Other Compose Files

Use Docker Compose's `include` directive to add the observability stack to your project (requires Docker Compose **v2.20+** / Docker Desktop **4.22+**):

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
  - RUST_LOG=info           # Use 'debug' for detailed troubleshooting
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

> **⚠️ Security Warning**: Default credentials are for development only. For production or network-exposed deployments:
> - Set `GRAFANA_ADMIN_PASSWORD` to a strong password
> - Bind to localhost only (`GRAFANA_BIND_HOST=127.0.0.1`) unless external access is required
> - Keep `GRAFANA_ANON_ENABLED=false` unless anonymous viewing is explicitly needed

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

## Security

### Development vs Production

This stack is designed for **development and testing environments**. The default configuration prioritizes ease of use over security:

- Services bind to `127.0.0.1` by default (localhost only)
- Elasticsearch has security disabled (`xpack.security.enabled=false`)
- Prometheus and Tempo have no authentication
- Default admin credentials are used

### Production Deployment

For production or network-exposed environments, consider:

1. **Strong Credentials**:
   ```bash
   export GRAFANA_ADMIN_PASSWORD="your-secure-password"
   ```

2. **External Access** (only if needed):
   ```bash
   export GRAFANA_BIND_HOST="0.0.0.0"
   ```

3. **Enable Security Features**:
   - Enable Elasticsearch security (xpack)
   - Use reverse proxy with authentication (nginx, Traefik)
   - Configure firewall rules
   - Use TLS/SSL certificates

4. **Network Isolation**:
   - Use Docker networks to isolate services
   - Only expose necessary ports
   - Use VPN for remote access

## Cleanup

Remove all containers and data:
```bash
docker compose down -v
```
