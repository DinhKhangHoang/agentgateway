#!/usr/bin/env bash
# Reusable chat-completions probe for the agentgateway pilot.
#
# Usage: chat.sh <host> <path> <model> [max_tokens]
#   host   gateway host, e.g. 116.118.88.175.nip.io
#   path   request path, canonical (/v1/chat/completions) or legacy
#          (/maas/user-11374/openai/gpt-4o/v1/chat/completions)
#   model  value for the request-body `model` field
#
# Prints the HTTP status code and the full response body. Exits 0 if the code
# is 2xx, 1 otherwise, so it can be used directly in a loop or CI gate.
# The gateway serves a self-signed certificate, hence `curl -k`.
set -uo pipefail

if [ "$#" -lt 3 ]; then
  echo "usage: $0 <host> <path> <model> [max_tokens]" >&2
  exit 2
fi

HOST="$1"
REQ_PATH="$2"
MODEL="$3"
MAX_TOKENS="${4:-10}"

BODY_FILE="$(mktemp)"
trap 'rm -f "$BODY_FILE"' EXIT

CODE="$(curl -sk -o "$BODY_FILE" -w '%{http_code}' -m 60 \
  -X POST "https://${HOST}${REQ_PATH}" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"say OK\"}],\"max_tokens\":${MAX_TOKENS}}")"

echo "POST https://${HOST}${REQ_PATH}  model=${MODEL}"
echo "HTTP ${CODE}"
cat "$BODY_FILE"
echo

case "$CODE" in
  2??) exit 0 ;;
  *)   exit 1 ;;
esac
