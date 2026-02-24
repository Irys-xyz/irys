#!/usr/bin/env sh
# Setup Grafana correlations (Elasticsearch logs → Tempo traces)
set -eu

GRAFANA_URL="${GRAFANA_URL:-http://grafana:3000}"
GRAFANA_USER="${GRAFANA_USER:-admin}"
GRAFANA_PASSWORD="${GRAFANA_PASSWORD:-admin}"
MAX_RETRIES=30
RETRY_INTERVAL=5

echo "Configuring Grafana correlations..."

echo "Waiting for Grafana to be ready..."
i=1
while [ "$i" -le "$MAX_RETRIES" ]; do
    if curl -s "$GRAFANA_URL/api/health" | grep -q '"database":"ok"'; then
        echo "Grafana is ready"
        break
    fi
    if [ "$i" -eq "$MAX_RETRIES" ]; then
        echo "Grafana not ready after $MAX_RETRIES attempts"
        exit 1
    fi
    echo "Waiting for Grafana... ($i/$MAX_RETRIES)"
    sleep "$RETRY_INTERVAL"
    i=$((i + 1))
done

AUTH="$GRAFANA_USER:$GRAFANA_PASSWORD"

# Check if correlation already exists
existing=$(curl -s "$GRAFANA_URL/api/datasources/correlations" -u "$AUTH" 2>/dev/null)
if echo "$existing" | grep -q '"field":"trace.id"'; then
    echo "Correlation (Elasticsearch trace.id → Tempo) already exists"
    exit 0
fi

# Create correlation: Elasticsearch trace.id → Tempo trace lookup
echo "Creating correlation: Elasticsearch trace.id → Tempo..."
response=$(curl -s -w "\n%{http_code}" -X POST \
  "$GRAFANA_URL/api/datasources/uid/elasticsearch/correlations" \
  -u "$AUTH" \
  -H 'Content-Type: application/json' \
  -d '{
    "targetUID": "tempo",
    "label": "View trace in Tempo",
    "description": "Open trace from trace ID in log entry",
    "type": "query",
    "config": {
      "field": "trace.id",
      "target": {
        "query": "${traceId}",
        "queryType": "traceql"
      },
      "transformations": [
        {
          "type": "regex",
          "expression": "(.*)",
          "mapValue": "traceId"
        }
      ]
    }
  }')

http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | sed '$d')

if [ "$http_code" -ge 200 ] && [ "$http_code" -lt 300 ]; then
    echo "Correlation created successfully"
else
    echo "Failed to create correlation (HTTP $http_code): $body"
    exit 1
fi

echo "Grafana correlation setup complete"
