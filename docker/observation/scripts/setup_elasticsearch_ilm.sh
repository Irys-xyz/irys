#!/usr/bin/env sh
# Setup Elasticsearch ILM policy for configurable log retention
set -eu

ES_HOST="${ES_HOST:-http://elasticsearch:9200}"
RETENTION_DAYS="${RETENTION_DAYS:-2}"
# Replica count: 0 for single-node/dev, increase for multi-node production clusters
ES_REPLICAS="${ES_REPLICAS:-0}"
CONFIG_DIR="${CONFIG_DIR:-/configs}"
MAX_RETRIES=30
RETRY_INTERVAL=5

echo "Configuring Elasticsearch with ${RETENTION_DAYS}-day retention, ${ES_REPLICAS} replica(s)..."

echo "Waiting for Elasticsearch to be ready..."
i=1
while [ "$i" -le "$MAX_RETRIES" ]; do
    if curl -s "$ES_HOST/_cluster/health" | grep -qE '"status":"(green|yellow)"'; then
        echo "Elasticsearch is ready"
        break
    fi
    if [ "$i" -eq "$MAX_RETRIES" ]; then
        echo "Elasticsearch not ready after $MAX_RETRIES attempts"
        exit 1
    fi
    echo "Waiting for Elasticsearch... ($i/$MAX_RETRIES)"
    sleep "$RETRY_INTERVAL"
    i=$((i + 1))
done

# Create ILM policy with configurable retention
echo "Creating ILM policy 'logs-retention'..."
response=$(sed "s/\${RETENTION_DAYS}/$RETENTION_DAYS/g" "$CONFIG_DIR/ilm-policy.json" | \
  curl -s -w "\n%{http_code}" -X PUT "$ES_HOST/_ilm/policy/logs-retention" \
    -H 'Content-Type: application/json' \
    -d @-)
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | sed '$d')
if [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
    echo "Failed to create ILM policy (HTTP $http_code): $body"
    exit 1
fi
echo "ILM policy created successfully"
echo ""

# Create ingest pipeline to flatten TraceId for Grafana data links
echo "Creating ingest pipeline 'add-flat-traceid'..."
response=$(curl -s -w "\n%{http_code}" -X PUT "$ES_HOST/_ingest/pipeline/add-flat-traceid" \
  -H 'Content-Type: application/json' \
  -d '{"description":"Copy TraceId to flat traceID for Grafana data links","processors":[{"set":{"field":"traceID","copy_from":"TraceId","if":"ctx.TraceId != null && ctx.TraceId.length() > 0"}}]}')
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | sed '$d')
if [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
    echo "Failed to create ingest pipeline (HTTP $http_code): $body"
    exit 1
fi
echo "Ingest pipeline created successfully"
echo ""

# Create index template that applies the ILM policy to Irys logs indices
echo "Creating index template 'irys-logs-template'..."
response=$(sed "s/__ES_REPLICAS__/$ES_REPLICAS/g" "$CONFIG_DIR/index-template.json" | \
  curl -s -w "\n%{http_code}" -X PUT "$ES_HOST/_index_template/irys-logs-template" \
    -H 'Content-Type: application/json' \
    -d @-)
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | sed '$d')
if [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
    echo "Failed to create index template (HTTP $http_code): $body"
    exit 1
fi
echo "Index template created successfully"
echo ""

# Create the initial irys-logs-000001 index with write alias
echo "Creating initial 'irys-logs-000001' index with 'irys-logs' write alias..."
response=$(sed "s/__ES_REPLICAS__/$ES_REPLICAS/g" "$CONFIG_DIR/index-settings.json" | \
  curl -s -w "\n%{http_code}" -X PUT "$ES_HOST/irys-logs-000001" \
    -H 'Content-Type: application/json' \
    -d @-)
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | sed '$d')
if [ "$http_code" -eq 200 ]; then
    echo "Index created successfully"
elif [ "$http_code" -eq 400 ] && echo "$body" | grep -q "resource_already_exists_exception"; then
    echo "(index already exists)"
elif [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
    echo "Failed to create index (HTTP $http_code): $body"
    exit 1
fi
echo ""

echo "Elasticsearch ILM setup complete - logs retained for ${RETENTION_DAYS} days with ${ES_REPLICAS} replica(s)"
