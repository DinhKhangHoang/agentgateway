#!/usr/bin/env bash
# Usage: chat.sh <host> <legacy-path> <model>
set -uo pipefail
HOST="$1"; P="$2"; MODEL="$3"
code=$(curl -sk -o /tmp/resp.json -w '%{http_code}' \
  -X POST "https://${HOST}${P}" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"say OK\"}],\"max_tokens\":10}")
echo "HTTP $code"
cat /tmp/resp.json; echo
jq -e '.choices[0].message.content' /tmp/resp.json
