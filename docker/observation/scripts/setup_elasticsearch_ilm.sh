#!/usr/bin/env bash
# Setup Elasticsearch ILM policy for configurable log retention
set -euo pipefail

ES_HOST="${ES_HOST:-http://elasticsearch:9200}"
RETENTION_DAYS="${RETENTION_DAYS:-7}"
MAX_RETRIES=30
RETRY_INTERVAL=5

echo "Configuring Elasticsearch with ${RETENTION_DAYS}-day retention..."

echo "Waiting for Elasticsearch to be ready..."
for i in $(seq 1 "$MAX_RETRIES"); do
    if curl -s "$ES_HOST/_cluster/health" | grep -q '"status":"green"\|"status":"yellow"'; then
        echo "Elasticsearch is ready"
        break
    fi
    if [ "$i" -eq "$MAX_RETRIES" ]; then
        echo "Elasticsearch not ready after $MAX_RETRIES attempts"
        exit 1
    fi
    echo "Waiting for Elasticsearch... ($i/$MAX_RETRIES)"
    sleep "$RETRY_INTERVAL"
done

# Create ILM policy with configurable retention
echo "Creating ILM policy 'logs-${RETENTION_DAYS}d-retention'..."
curl -s -X PUT "$ES_HOST/_ilm/policy/logs-retention" \
  -H 'Content-Type: application/json' \
  -d "{
    \"policy\": {
      \"phases\": {
        \"hot\": {
          \"min_age\": \"0ms\",
          \"actions\": {
            \"rollover\": {
              \"max_age\": \"1d\",
              \"max_primary_shard_size\": \"10gb\"
            }
          }
        },
        \"delete\": {
          \"min_age\": \"${RETENTION_DAYS}d\",
          \"actions\": {
            \"delete\": {}
          }
        }
      }
    }
  }"
echo ""

# Create index template that applies the ILM policy to Irys logs indices
echo "Creating index template 'irys-logs-template'..."
curl -s -X PUT "$ES_HOST/_index_template/irys-logs-template" \
  -H 'Content-Type: application/json' \
  -d '{
    "index_patterns": ["irys-logs*"],
    "template": {
      "settings": {
        "index.lifecycle.name": "logs-retention",
        "number_of_shards": 1,
        "number_of_replicas": 0
      }
    },
    "priority": 200
  }'
echo ""

# Create the initial irys-logs index (single-node: 0 replicas)
echo "Creating initial 'irys-logs' index..."
response=$(curl -s -w "\n%{http_code}" -X PUT "$ES_HOST/irys-logs" \
  -H 'Content-Type: application/json' \
  -d '{
    "settings": {
      "index.lifecycle.name": "logs-retention",
      "number_of_shards": 1,
      "number_of_replicas": 0
    }
  }')
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

echo "Elasticsearch ILM setup complete - logs will be retained for ${RETENTION_DAYS} days"
