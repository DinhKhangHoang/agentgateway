# Runbook — agentgateway Operations

**Date:** 2026-09-14
**Scope:** agentgateway dataplane + ai-gateway-plugin-server policy plane + GPU backends (SGLang).

---

## Common failure modes

| Failure | Symptom | Detection | Immediate action |
|---|---|---|---|
| **Upstream 5xx** | Client receives 500/502/503 from agentgateway | `agw_balance_picks_total{result="selected"}` drops; upstream 5xx in access logs | Check SGLang router health; if one worker, SGLang CB removes it; if whole farm, agentgateway fails over to next farm or provider |
| **Evicted endpoint (soft-degrade)** | Requests still succeed but latency rises; `agw_balance_exhausted_total` increments | `agw_health_eviction_total` rising; `agw_balance_picks_total{result="skipped_evicted"}` rising | Investigate why endpoints are being evicted (see G4 prober lifecycle below); check upstream health |
| **NoHealthyEndpoints 503 + Retry-After** | Client receives 503 with `Retry-After` header | `agw_balance_picks_total{result="no_endpoints"}` increments | This fires only at **zero endpoints total** (empty backend group, not just all-evicted). Check `kubectl get agentgatewaybackends` — the backend group is misconfigured or all endpoints were removed. |
| **Ext-auth failure (503)** | Client receives 503 `{"message":"Gateway Auth Unavailable"}` | Policy server `/health` or `/ready` failing; `ai_gateway_decisions_total{allowed="false",reason="error"}` rising | Check policy server pod health, Redis/Dragonfly connectivity. Fail-closed by design — no traffic passes unauthenticated. |
| **xDS stream disconnect** | Dataplane config stale; new CRDs not taking effect | Controller logs: `xDS stream error` / `connection reset` | Restart controller pod; check `kubectl logs -n <ns> <controller-pod>` for reconcile errors |
| **Prober false positives** | Healthy endpoints evicted; `agw_health_eviction_total{source="probe"}` rising without real failures | `agw_health_probe_total{result="failure"}` rising but real-request success rate normal | Check probe timeout vs. farm latency; increase `activeProbe.timeout` or `activeProbe.consecutiveFailures` |

---

## Diagnostic commands

### Check dataplane health

```bash
# Pod status
kubectl get pods -n <ns> -l app=agentgateway

# Recent logs (RUST_LOG controls verbosity)
kubectl logs -n <ns> -l app=agentgateway --tail=200
# For detailed routing/balancing logs:
kubectl logs -n <ns> -l app=agentgateway --tail=200 -c agentgateway | grep -iE 'balance|health|evict|failover'
# For ext-auth logs:
kubectl logs -n <ns> -l app=agentgateway --tail=200 -c agentgateway | grep -iE 'ext.auth|check|deny|503'
```

### Check policy server health

```bash
# Pod status
kubectl get pods -n <ns> -l app=ai-gateway-plugin-server

# Logs
kubectl logs -n <ns> -l app=ai-gateway-plugin-server --tail=200
# For auth/RL details:
kubectl logs -n <ns> -l app=ai-gateway-plugin-server --tail=200 | grep -iE 'check|deny|rate.limit|429|503'

# Readiness (should be 200; 503 = Redis/Dragonfly down)
kubectl exec -n <ns> <policy-server-pod> -- curl -s http://localhost:8080/ready
```

### Check model/backend CRDs

```bash
# All models
kubectl get agentgatewaymodels -A

# All backends
kubectl get agentgatewaybackends -A

# All policies
kubectl get agentgatewaypolicies -A

# Detailed view of a specific model
kubectl describe agentgatewaymodel <name> -n <ns>
```

### Prometheus metric queries

**WS-5 balance & health metrics:**

```promql
# Load-balancer pick results (should be mostly "selected")
sum by (result) (rate(agw_balance_picks_total[5m]))

# Soft-degrade events (should be near zero; rising = all endpoints evicted in a backend)
sum by (backend) (rate(agw_balance_exhausted_total[5m]))

# Health probe results
sum by (result) (rate(agw_health_probe_total[5m]))

# Evictions by source (probe vs real_request)
sum by (source) (rate(agw_health_eviction_total[5m]))

# Eviction rate per endpoint (identify sick backends)
sum by (backend, endpoint) (rate(agw_health_eviction_total[5m]))
```

**Policy server metrics:**

```promql
# Auth decision rate
sum by (allowed) (rate(ai_gateway_decisions_total[5m]))

# Check latency p99
histogram_quantile(0.99, sum by (le) (rate(ai_gateway_http_requests_duration_seconds_bucket{path="/v1/check"}[5m])))

# Rate-limit denials
sum by (reason) (rate(ai_gateway_decisions_total{allowed="false",reason="429"}[5m]))

# Redis EVAL latency p99
histogram_quantile(0.99, sum by (le) (rate(ai_gateway_redis_eval_duration_seconds_bucket[5m])))
```

### Check SGLang router

```bash
# SGLang health
curl http://<sglang-router>:30000/health

# SGLang load (per-worker queue depth, cache stats)
curl http://<sglang-router>:30000/v1/loads | jq .
```

---

## G4 prober lifecycle

The active health prober (G4, FR-5.1–5.3) runs as a background `tokio::task` per backend. Understanding its lifecycle is essential for diagnosing eviction behavior.

### Spawn (lazy)

The prober does **not** start at config load. It spawns **lazily on the first authorized request** to the backend. This avoids probing backends that have no traffic (e.g., a standby farm).

```
config load → no prober
first authorized request to backend → prober task spawns
```

### Probe (interval)

Every `activeProbe.interval` (default 30s), the prober sends a tiny request (`max_tokens: 1`) to each endpoint in the backend. Each probe has a `activeProbe.timeout` (default 5s) deadline.

```
prober loop:
  sleep(interval)
  for each endpoint:
    send probe (max_tokens=1, timeout=activeProbe.timeout)
    if success: record success (EWMA health up)
    if failure/timeout: record failure (EWMA health down)
```

Outcomes feed the same `Ewma::record()` + eviction path as real traffic.

### Evict (consecutive_failures)

After `activeProbe.consecutiveFailures` (default 2) **consecutive** probe failures, the endpoint is evicted from the active set for `eviction.duration` (default 3s, or `Retry-After` header, or retry backoff). This is independent of `eviction.consecutiveFailures` (which counts real-request failures); both feed the same eviction decision.

```
endpoint health drops below threshold
  AND consecutive probe failures >= activeProbe.consecutiveFailures
  OR consecutive real-request failures >= eviction.consecutiveFailures
→ evict endpoint for eviction.duration
```

### Unevict (eviction.duration)

After `eviction.duration` expires, the endpoint returns to the active set. Its health score is restored to `eviction.restoreHealth` (if configured; otherwise left unchanged for gradual recovery). The prober continues probing — a successful probe restores health further.

### Kill-switch (generation stale on config change)

Each prober task carries a **generation** counter. On config reload (CRD update → xDS push → dataplane config update), the old prober's generation becomes stale and the task exits. A new prober starts with the new config.

```
config reload → generation increments
old prober task: generation != current → exit
new prober task: spawns with new config (lazy, on next request)
```

This prevents stale probers from evicting endpoints based on old health policies (e.g., a removed backend, a changed `consecutiveFailures` threshold).

---

## Key insight: NoHealthyEndpoints 503 is unreachable via health eviction

**The 503 + Retry-After response is NOT caused by health eviction on the AI path.**

When all endpoints in a backend group are evicted, agentgateway does **soft-degrade**: it reuses the evicted endpoints (with reduced health score) rather than returning 503. The request still goes through — it may hit a degraded endpoint, but it is not rejected.

The 503 fires only when there are **zero endpoints total** in the backend group. This happens when:

- The backend group is empty (misconfigured CRD, all `AgentgatewayBackend` entries removed).
- The backend group was never populated (DNS resolution failed for all endpoints at config load).
- The backend group has been removed entirely from the config.

**Implication for ops:** if you see 503 with `Retry-After`, do not investigate health probing or eviction — check the backend group configuration. The relevant metric is `agw_balance_picks_total{result="no_endpoints"}`, not `agw_health_eviction_total`.

**Implication for SLOs:** health probing protects against routing to dead endpoints (proactive eviction) but never causes a hard outage on its own. The risk is degraded latency (routing to a sick endpoint via soft-degrade), not availability. Monitor `agw_balance_exhausted_total` (soft-degrade rate) as a latency-risk signal, not an availability signal.

---

## Rolling updates

### Dataplane image roll

```bash
# Patch the GatewayConfiguration image (safe: targeted JSON patch)
kubectl patch gatewayconfiguration <name> -n <ns> --type=json \
  -p '[{"op":"replace","path":"/spec/dataPlaneOptions/deployment/podTemplateSpec/spec/containers/0/image","value":"<new-tag>"}]'

# Bump the watcher annotation to trigger reconcile
kubectl annotate gatewayconfiguration <name> -n <ns> \
  kgo.custom.watcher/restartedAt=$(date +%s) --overwrite
```

**Watch for:**
- Transient 503 `Gateway Auth Unavailable` for a few seconds (policy server reconnecting) — retries clear it.
- xDS stream disconnect during rollout — controller pushes new config to new pods automatically.

### Policy server image roll

```bash
kubectl set image deployment/ai-gateway-plugin-server \
  ai-gateway-plugin-server=<new-image> -n <ns>
```

**Watch for:**
- Fail-closed 503 during rollout — if the policy server is down, all traffic returns 503. Roll one pod at a time (default rolling update) to maintain availability.

### Config change (CRD update)

```bash
kubectl apply -f <model-or-policy-crd.yaml>
```

The controller picks up the change, pushes it via xDS to the dataplane. The prober kill-switch fires on config reload — old prober exits, new prober spawns lazily on the next request.

**Watch for:**
- Non-additive schema changes may be rejected by the validating webhook if it validates against the old dataplane during a rolling update. Bridge-deploy: apply with the changed field removed → roll the image → apply the full new config.

---

## Redis / DragonflyDB

### Check connectivity

```bash
# From policy server pod
kubectl exec -n <ns> <policy-server-pod> -- redis-cli -h <redis-host> ping
# Should return PONG

# From agentgateway pod
kubectl exec -n <ns> <agentgateway-pod> -- redis-cli -h <redis-host> ping
```

### Redis down

- **agentgateway:** browns out to per-instance local state. Sticky affinity degrades (may cause cache cold starts), quotas degrade (may briefly over-allow). No outage.
- **Policy server:** fail-closed 503. All traffic rejected. **This is the highest-priority incident** — no traffic passes unauthenticated.

### Recovery

```bash
# Check Redis cluster health
redis-cli -h <redis-host> cluster info
redis-cli -h <redis-host> cluster nodes

# If DragonflyDB, check via its admin port
curl http://<dragonfly-host>:<admin-port>/stats
```

---

## Quick verification

```bash
# Verify a request flows end-to-end
curl -v http://<agentgateway-host>:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <api-key>" \
  -d '{"model":"<model-name>","messages":[{"role":"user","content":"hello"}]}'

# Check model list
curl http://<agentgateway-host>:4000/v1/models \
  -H "Authorization: Bearer <api-key>"

# Verify metrics are moving
curl http://<agentgateway-host>:19002/metrics | grep -E 'agw_balance_picks_total|agw_health_probe_total'
```
