#!/usr/bin/env bash
# Acceptance sweep for the agentgateway MaaS-v2 pilot (design spec §10).
#
# Usage:
#   GW_HOST=116.118.88.175.nip.io ./acceptance.sh
#   GW_HOST=... PILOT_API_KEY=<key> ./acceptance.sh
#
# PILOT_API_KEY is optional. Without it the authenticated criteria are reported
# SKIP rather than silently passing — a sweep that cannot test the allow path
# must say so, not pretend.
#
# Exit status: 0 only if there are no FAILs. SKIPs do not fail the run, but they
# are counted and printed so an incomplete sweep is never mistaken for a green one.
set -uo pipefail

HOST="${GW_HOST:?set GW_HOST}"
KEY="${PILOT_API_KEY:-}"
pass=0; fail=0; skip=0

ok()   { printf 'PASS  %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf 'FAIL  %s\n      %s\n' "$1" "$2"; fail=$((fail+1)); }
skipf(){ printf 'SKIP  %s\n      %s\n' "$1" "$2"; skip=$((skip+1)); }

auth_args() { [ -n "$KEY" ] && printf '%s' "-H|Authorization: Bearer ${KEY}"; }

# call <path> <model> [content] -> prints HTTP code, body lands in /tmp/acc.json
call() {
  local path="$1" model="$2" content="${3:-say OK}"
  if [ -n "$KEY" ]; then
    curl -sk -o /tmp/acc.json -w '%{http_code}' -m 90 -X POST "https://${HOST}${path}" \
      -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
      -d "{\"model\":\"${model}\",\"messages\":[{\"role\":\"user\",\"content\":\"${content}\"}],\"max_tokens\":10}"
  else
    curl -sk -o /tmp/acc.json -w '%{http_code}' -m 90 -X POST "https://${HOST}${path}" \
      -H 'Content-Type: application/json' \
      -d "{\"model\":\"${model}\",\"messages\":[{\"role\":\"user\",\"content\":\"${content}\"}],\"max_tokens\":10}"
  fi
}

body() { head -c 200 /tmp/acc.json; }

echo "=== agentgateway MaaS-v2 pilot acceptance sweep ==="
echo "host=${HOST}  authenticated=$([ -n "$KEY" ] && echo yes || echo no)"
echo

# extAuth is live and fail-closed. Without a registry-issued key every request
# is denied at the gateway, so no model-path criterion can be evaluated. Report
# that honestly and stop, rather than emitting a wall of misleading FAILs.
if [ -z "$KEY" ]; then
  u=$(curl -sk -o /tmp/acc.json -w '%{http_code}' -m 60 -X POST \
        "https://${HOST}/v1/chat/completions" -H 'Content-Type: application/json' \
        -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":5}')
  if [ "$u" = "200" ]; then
    bad "unauthenticated request ALLOWED" "SECURITY: got 200 — extAuth is not enforcing"
  else
    ok "unauthenticated request rejected (got $u)"
  fi
  echo
  skipf "all model-path criteria" "no PILOT_API_KEY; extAuth denies every request before the model"
  echo
  echo "----"
  echo "passed=$pass failed=$fail skipped=$skip"
  echo
  echo "To run the full sweep, export PILOT_API_KEY with a key issued by the"
  echo "tenant registry at pub-iamapis.api-dev.vngcloud.tech."
  [ "$fail" -eq 0 ]
  exit $?
fi

# ---------------------------------------------------------------------------
# Criterion 1 — models respond on the CANONICAL surface (AgentgatewayModel)
# ---------------------------------------------------------------------------
echo "-- canonical surface (/v1/chat/completions, model from body) --"

c=$(call /v1/chat/completions gemini-2.5-flash)
[ "$c" = "200" ] && ok "gemini-2.5-flash canonical -> 200" \
                 || bad "gemini-2.5-flash canonical" "got $c: $(body)"

c=$(call /v1/chat/completions deepseek-v4-pro)
[ "$c" = "200" ] && ok "deepseek-v4-pro (weighted virtual model) -> 200" \
                 || bad "deepseek-v4-pro canonical" "got $c: $(body)"

# gpt-4o carries a known-dummy BYOK key in this dev cluster: reaching OpenAI at
# all is the success condition, so a 401 originating upstream is a PASS.
c=$(call /v1/chat/completions gpt-4o)
if [ "$c" = "401" ] && grep -qi 'api key' /tmp/acc.json; then
  ok "gpt-4o reaches api.openai.com (401 from dummy BYOK key, as expected)"
elif [ "$c" = "200" ]; then
  ok "gpt-4o canonical -> 200 (key must have been replaced)"
else
  bad "gpt-4o canonical" "got $c: $(body)"
fi

# Anthropic account is out of credit; routing is still provable.
c=$(call /v1/chat/completions claude-sonnet-4-0)
if [ "$c" = "200" ]; then
  ok "claude-sonnet-4-0 canonical -> 200"
elif grep -qi 'credit balance' /tmp/acc.json; then
  skipf "claude-sonnet-4-0 canonical" "routing OK; upstream account has no credit (got $c)"
else
  bad "claude-sonnet-4-0 canonical" "got $c: $(body)"
fi

# ---------------------------------------------------------------------------
# Criterion 2 — legacy Kong URLs (AgentgatewayBackend via HTTPRoute backendRefs)
# ---------------------------------------------------------------------------
echo
echo "-- legacy Kong URL surface --"

c=$(call /maas/user-11374/gemini/gemini-2.5-flash/v1/chat/completions gemini-2.5-flash)
[ "$c" = "200" ] && ok "legacy gemini chat -> 200" \
                 || bad "legacy gemini chat" "got $c: $(body)"

c=$(call /maas/user-11374/deepseek/deepseek-v4-pro/v1/chat/completions deepseek-v4-pro)
[ "$c" = "200" ] && ok "legacy deepseek chat -> 200" \
                 || bad "legacy deepseek chat" "got $c: $(body)"

# The /maas/user-N/ prefix is optional in Kong; the regex must accept both.
c=$(call /gemini/gemini-2.5-flash/v1/chat/completions gemini-2.5-flash)
[ "$c" = "200" ] && ok "legacy path without /maas/user-N prefix -> 200" \
                 || bad "legacy path without prefix" "got $c: $(body)"

# ---------------------------------------------------------------------------
# Criterion 3 — streaming
# ---------------------------------------------------------------------------
echo
echo "-- streaming --"
if [ -n "$KEY" ]; then
  n=$(curl -skN -m 90 -X POST "https://${HOST}/v1/chat/completions" \
        -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
        -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"count to 3"}],"stream":true,"max_tokens":20}' \
      | grep -c '^data:')
else
  n=$(curl -skN -m 90 -X POST "https://${HOST}/v1/chat/completions" \
        -H 'Content-Type: application/json' \
        -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"count to 3"}],"stream":true,"max_tokens":20}' \
      | grep -c '^data:')
fi
[ "${n:-0}" -gt 0 ] && ok "streaming emits ${n} SSE frames" \
                    || bad "streaming" "no 'data:' frames received"

# ---------------------------------------------------------------------------
# Criterion 4 — Internal models are not directly addressable
# ---------------------------------------------------------------------------
echo
echo "-- visibility --"
for m in deepseek-v4-pro-direct deepseek-v4-pro-dashscope; do
  c=$(call /v1/chat/completions "$m")
  [ "$c" != "200" ] && ok "Internal model $m not directly addressable (got $c)" \
                    || bad "Internal model $m IS directly addressable" "SECURITY: got 200"
done

# ---------------------------------------------------------------------------
# Criterion 5 — authorization
# ---------------------------------------------------------------------------
echo
echo "-- authorization --"
u=$(curl -sk -o /tmp/acc.json -w '%{http_code}' -m 60 -X POST \
      "https://${HOST}/v1/chat/completions" -H 'Content-Type: application/json' \
      -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":5}')
[ "$u" != "200" ] && ok "unauthenticated request rejected (got $u)" \
                  || bad "unauthenticated request ALLOWED" "SECURITY: got 200"

if [ -z "$KEY" ]; then
  skipf "authenticated request succeeds" "no PILOT_API_KEY set; allow path UNVERIFIED"
fi

echo
echo "----"
echo "passed=$pass failed=$fail skipped=$skip"
[ "$fail" -eq 0 ]
