# agentgateway — System Architecture

> **Audience:** SRE / platform ops — everything needed to understand, operate, and troubleshoot the system end-to-end.
> **Scope:** the `agentgateway` dataplane (Rust), the `ai-gateway-plugin-server` policy plane (Rust, ext-auth via gRPC), GPU backends (SGLang), and supporting services.
> **Status:** architecture snapshot, 2026-09-14.

---

## Table of Contents

1. [What this system is](#1-what-this-system-is)
2. [System topology](#2-system-topology)
3. [Component catalog](#3-component-catalog)
4. [The request lifecycle (end-to-end)](#4-the-request-lifecycle-end-to-end)
5. [Multi-model routing, balancing, failover & affinity](#5-multi-model-routing-balancing-failover--affinity)
6. [Identity, auth, ACL & rate limiting](#6-identity-auth-acl--rate-limiting)
7. [Health probing & eviction](#7-health-probing--eviction)
8. [The `web_search` server-tool subsystem](#8-the-web_search-server-tool-subsystem)
9. [Deployment topology](#9-deployment-topology)
10. [Security model](#10-security-model)
11. [Observability](#11-observability)
12. [Known issues & gotchas](#12-known-issues--gotchas)
13. [Glossary](#13-glossary)

---

## 1. What this system is

agentgateway is a multi-tenant, multi-model LLM serving platform. It sits between AI clients (IDEs/coding agents like Claude Code, OpenCode; OpenAI/Anthropic-compatible SDK clients) and a fleet of self-hosted GPU model backends plus online provider fallbacks.

**What it does for every LLM request:**

- **Authenticate** the caller by API key, **authorize** the tenant-to-model mapping, **enforce** RPM/TPM rate limits — all via a gRPC ext-auth call to the policy plane (fail-closed 503 if it cannot reach the policy server).
- **Route** the request to the right LLM backend — self-hosted GPU farm first, online provider as overflow — with session-to-farm cache affinity and cross-farm failover.
- **Normalize** between API shapes: OpenAI Chat Completions / Responses, Anthropic Messages, and image APIs — so clients can speak any of them against any backend that speaks any of them.
- **Stream** SSE transparently with minimal added latency.
- **Meter** token usage for TPM true-up and cost accounting.
- **Health-probe** backends actively (G4) and evict unhealthy endpoints before real traffic hits them.
- Optionally **inject server-side tools** (today: `web_search`) by hijacking the request into an in-pod loop that grounds the LLM's answer in live web data.

**The defining design decision:** identity and rate-limiting are offloaded from the dataplane to a dedicated Rust policy plane (`ai-gateway-plugin-server`) via ext-auth. The dataplane has no consumer objects — identity comes from the API key via the policy server. Route count scales with **models**, not tenants.

The two components:

- **agentgateway** (this repo) — the dataplane: a Rust proxy built on axum/tokio that handles LLM routing, balancing, failover, health probing, streaming, and protocol normalization.
- **ai-gateway-plugin-server** — the Rust ext-auth + metering policy server called per request, plus mock backends for benchmarking.

---

## 2. System topology

```
                            Internet (AI clients: IDEs, agents, SDKs)
                               │
                    ┌──────────▼──────────┐
                    │   L4 Load Balancer  │  TLS terminate or passthrough
                    │ (NLB/LVS/HAProxy)   │  source-IP hash, PROXY protocol, conn draining
                    └──────────┬──────────┘
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
        ┌──────────────┐ ┌──────────────┐ ┌──────────────┐  agentgateway dataplane pods (N×, HA)
        │ agentgateway │ │ agentgateway │ │ agentgateway │  image: agentgateway:latest
        │      #1      │ │      #2      │ │      #N      │
        └──────┬───────┘ └──────┬───────┘ └──────┬───────┘
               │  ┌─────────────┼─────────────────┘
               │  │
               │  ├─────► Redis / DragonflyDB (HA) ── shared state: sticky affinity, capacity,
               │  │        (policy-server RL counters)   cooldown, TPM windows, RL counters
               │  │
               │  ├─────► ai-gateway-plugin-server (Rust) ── /v1/check (auth+ACL+RL), /v1/usage (meter)
               │  │        per-request ext-auth, fail-closed 503, always-200 decision body
               │  │            │
               │  │            ├─► MAAS backend API (over HTTPS, OAuth2 IAM) ── key config/ACL source
               │  │            └─► SNS/SQS invalidation bus ── cache eviction events
               │  │
               │  └─────► Prometheus ──► Grafana dashboard
               │
        ╔══════════════════════════════ LLM backends ═══════════════════════════════╗
        ║                                                                                ║
        ║  Self-hosted GPU farms (the primary path):                                    ║
        ║   ┌─────────────────────────┐    ┌─────────────────────────┐                 ║
        ║   │  GPU Farm A             │    │  GPU Farm B             │  ...            ║
        ║   │  ┌───────────────────┐  │    │  ┌───────────────────┐  │                 ║
        ║   │  │ SGLang router     │  │    │  │ SGLang router     │  │  ← cache_aware  ║
        ║   │  │ (one per farm,HA) │  │    │  │ (one per farm,HA) │  │     worker pick ║
        ║   │  └────────┬──────────┘  │    │  └────────┬──────────┘  │                 ║
        ║   │    ┌──────┴──────┐      │    │    ┌──────┴──────┐      │                 ║
        ║   │    │ AC pool     │AG p. │    │    │ AC pool     │AG p. │  8×H100 workers ║
        ║   │    │(autocomplete)│(agentic)│    │(autocomplete)│(agentic)                ║
        ║   └─────────────────────────┘    └─────────────────────────┘                 ║
        ║                                                                                ║
        ║  Online providers (OVERFLOW + FAILOVER only, last resort):                    ║
        ║   OpenAI / Anthropic / OpenRouter / Gemini — normalized to OpenAI by drivers  ║
        ╚══════════════════════════════════════════════════════════════════════════════╝
```

**Golden rule (layer separation):** *agentgateway routes **between** failure domains (farms/providers); SGLang routes **within** a farm.* Never let one do the other's job. agentgateway owns authN/Z, per-tenant TPM/token budgets, farm-vs-provider routing, session-to-farm affinity, cross-farm failover, provider overflow, cost/token accounting. SGLang owns KV-cache-aware worker selection, intra-farm load balance, worker health/circuit-breaking. The worker (8×H100) owns inference + prefix KV cache + continuous batching.

---

## 3. Component catalog

### 3.1 Dataplane (agentgateway, Rust)

Built on axum 0.8 / tokio / hyper. Handles all HTTP request processing, LLM protocol normalization, routing, balancing, failover, health probing, and streaming.

**Config model:** CRDs — `AgentgatewayModel`, `AgentgatewayBackend`, `AgentgatewayPolicy`, `AgentgatewayParameter`. In standalone mode, a single `config.yaml` file (see `examples/*/config.yaml`).

**Key subsystems:**

- **LLMRouter** (`llm/model_router.rs`) — matches the incoming request's `model` field against configured model entries (exact name, wildcard `provider/*`). Resolves to a backend reference + LLM policy. Supports virtual models with weighted, failover, or conditional routing.
- **AIBackend** (`llm/mod.rs`) — a set of named AI providers with power-of-two load balancing. Each provider carries a host override, path override, inline policies, and optional tokenization.
- **Drivers** (`llm/{openai,anthropic,gemini,vertex,bedrock,azure,copilot,custom}/`) — one per LLM provider. Each handles request/response format conversion, streaming, and provider-specific quirks.
- **Health prober** (`http/health_prober.rs`) — G4 active health probing (FR-5.1–5.3). Background task that periodically probes each endpoint and feeds outcomes into the eviction machinery.
- **Route policies** (`http/{timeout,retry,health,buffer,delay,...}.rs`) — per-route policies for timeout, retry, health/eviction, buffering, rate limiting, CORS, header modification, URL rewrite, and more.

### 3.2 Policy plane (ai-gateway-plugin-server, Rust)

Rust (axum 0.8 / tokio / fred / moka), fail-closed. The dataplane calls `/v1/check` per request and `/v1/usage` after. Detailed in [§6](#6-identity-auth-acl--rate-limiting).

Layered Clean Architecture: `http → pipeline → domain`, with `pipeline` depending on `infra` only through trait ports (so the engine is unit-testable with no Redis/WireMock).

### 3.3 GPU backends & SGLang

- **SGLang router** (one per farm, HA active-standby) — `--policy cache_aware` (KV-cache-aware worker selection via a UTF-8 radix tree + in-flight-count balance gate), separate **AC (autocomplete)** vs **AG (agentic)** worker pools.
- **Workers** — 8×H100 nodes running MoE models (MiniMax-M2 ~230B total/~10B active, plus GLM-5.2, gemma-4-31b-it).
- **Online providers** — OpenAI / Anthropic / OpenRouter / Gemini as last-resort overflow+failover, normalized to OpenAI shape by the drivers.

### 3.4 Supporting services

- **Redis / DragonflyDB** (HA) — cross-instance shared state: sticky affinity bindings, failover cooldown, farm capacity counters, TPM windows; and the policy server's rate-limit counters (Dragonfly is the RL store; Redis remains the affinity store).
- **MAAS backend API** — source of truth for key config, ACL, rate-limit budgets, guardrails. The policy server caches it (moka) and refreshes via TTL + SNS push invalidation.
- **SNS/SQS invalidation bus** — AWS push events that evict policy-server cache entries by key/tenant/group.
- **Prometheus / Grafana** — scrapes both the policy server `/metrics` and agentgateway metrics; sizing + ops dashboards.

---

## 4. The request lifecycle (end-to-end)

### 4.1 Normal LLM request — full chain

1. **Client** (IDE/agent/SDK) → **L4 LB** (TLS, source-IP hash, PROXY protocol) → **an agentgateway instance** (any; state is in Redis).
2. **Ext-auth** — agentgateway calls the policy server `/v1/check` `{api_key, model, estimated_tokens, stages, path_tenant}`. On **deny** → `kong.response.exit(status_code)` (401/403/429/503) with `X-RateLimit-*` + `Retry-After`. On **allow** → proceed. **Fail-closed**: transport error / non-200 / unparseable body → **503**.
3. **Model resolution** — LLMRouter matches the request's `model` field against configured model entries. Resolves to a backend reference (provider + host + port) and LLM policy.
4. **Balancing** — if the backend has multiple providers/endpoints, select one via power-of-two load balancing, excluding evicted/cooldown endpoints. Sticky-first: look up affinity key → farm binding in Redis.
5. **Request transformation** — normalize the request body from the client's API shape (OpenAI/Anthropic/Responses) to the target provider's format. Apply any configured request header modifications, URL rewrites, or CEL transformations.
6. **Forward to upstream** — the chosen driver's `configure_request` sets the real upstream (host, port, path, headers, query) and agentgateway proxies the request.
7. **Upstream LLM responds.**
8. **Response processing** — if streaming (SSE): per-chunk normalization from the provider's format to the client's API shape. If buffered: whole-body normalization once.
9. **Log + meter** — record token usage, compute `delta = actual − estimated`, fire-and-forget `POST /v1/usage` to the policy server for TPM true-up. Emit structured audit log.

**Failover (in-request):** on an HTTP status in the retry policy's `codes` list (typically `[500, 502, 503, 504]` + TCP errors), agentgateway benches the failed endpoint, re-picks a target, and re-issues — bounded by the retry `attempts` count. Chain: bound farm → other farms → online provider → error. **Never retry mid-stream** (avoids duplicate/partial generations); rely on the failover chain pre-first-token only.

### 4.2 `web_search` request

When a request carries a server tool in `tools[]` AND web search is enabled on the target model, agentgateway reroutes the request to an in-pod sidecar that runs the agentic search loop. The sidecar talks to the GPU farm (url+auth forwarded per request), runs search rounds, and returns a grounded, cited response. agentgateway renders the provenance into the client's API shape (Anthropic `server_tool_use`/`web_search_tool_result`/`citations_delta`, or OpenAI Responses `web_search_call`/`output_text`/`url_citation`).

---

## 5. Multi-model routing, balancing, failover & affinity

### 5.1 Model routing

Each model entry in the config maps a model name (exact or wildcard `provider/*`) to a backend provider. Virtual models can route to multiple concrete models with weighted, failover, or conditional routing.

**Traffic-class split:** autocomplete (`/v1/.../completions`, small prompt big-%-cached, TTFT p95 < ~300ms, AC worker pool) vs agentic (`/v1/chat/completions`, 30k–200k tok multi-turn, throughput, AG pool). agentgateway maps the two routes to two upstream targets so their queues/timeouts/SLOs stay independent — a 200k agentic prefill must not head-of-line-block an autocomplete.

### 5.2 Balancing

Power-of-two load balancing across endpoints, excluding endpoints in eviction cooldown or already tried this request. Sticky-first: look up affinity key (`hash(API-key ID [+ client IP, optional])`) → farm binding in Redis. This keeps an identity's traffic on the farm whose SGLang prefix cache is warm. **Use the API-key ID, never the raw secret.**

### 5.3 Failover chain

Bound farm → other farms (on 5xx/timeout, cooldown in Redis) → **online provider** (normalized to OpenAI by the driver) → error. Provider overflow is last-resort and cost-capped by a dedicated TPM budget. Never retry mid-stream.

### 5.4 Capacity awareness

Three gates before feeding a farm:

1. **`concurrent`** — max concurrent streams/farm.
2. **`tpm`** — tokens/min/farm.
3. **`prometheus`** — a custom Prometheus query against the farm (e.g. KV-cache usage %) with a soft threshold (~85%).

If any gate trips → the farm is "saturated" → pick another (least-loaded) and rebind the key.

### 5.5 Capacity laws

```
AG_workers = ceil(peak_concurrent_AG_sessions / (AG_req_s_per_worker × avg_AG_latency_s))
AC_workers = ceil(peak_AC_rps / AC_req_s_per_worker)
workers_per_farm = AG_workers + AC_workers + ceil(0.2 × total)   # +20% headroom/HA
N_farms    = max(availability_zones, ceil(total_workers / workers_per_router_comfortably))
N_agw      = ceil(peak_concurrent_streams / streams_per_agw) + 1   # N+1 HA
```

Bottlenecks at sane scale are **GPU decode throughput** (workers) and **concurrent SSE stream count** (agentgateway) — not L4 or Redis.

---

## 6. Identity, auth, ACL & rate limiting

### 6.1 The pattern — ext-authz, fail-closed, always-200

agentgateway calls the policy server per request via gRPC ext-auth. The policy server **never touches LLM request/response bodies or SSE streams**; it only guards and meters. The whole system is **fail-closed**: if the policy server is unreachable, returns non-200, or returns an unparseable body, agentgateway returns **503** — no traffic reaches the GPU farms unauthenticated.

**The always-200 contract:** `/v1/check` returns HTTP 200 on every code path, with allow/deny encoded in the JSON body (`allowed`, `status_code`, `reason`, `tenant_id`, `rate_limits[]`, `guardrails`). Every error is mapped to a 200 deny, never a 4xx/5xx.

### 6.2 The 4-stage enforcement pipeline

1. **`authn`** — cache-aside lookup by SHA-256 of the raw key. On cache miss → `GET {backend}/v1/keys/{sha256}`. Unknown/inactive/expired key → 401; backend outage → 503; path-tenant mismatch → 403.
2. **`acl`** — pure predicate: is the model in the union of `allowed_models` + `byok_models`? Else 403.
3. **`ratelimit`** — flattens key/tenant/group/model × RPM/TPM × multi-window config into one counter list and runs an atomic check-then-increment. Breach → 429.
4. **`guardrails`** — attach-only, no enforcement: surfaces the model's directive list on the allow response.

### 6.3 Two-phase TPM true-up

Token quotas (TPM) can only be enforced at this layer — SGLang cannot. Because `/v1/check` runs before the LLM responds, it uses an estimated token count; after the response, agentgateway reads the actual token usage, computes `delta = actual − estimated`, and fire-and-forget `POST /v1/usage` applies the delta to the same TPM counters. A lost usage report self-corrects on the next request.

---

## 7. Health probing & eviction

### 7.1 Active health prober (G4, FR-5.1–5.3)

A background `tokio::task` per backend that periodically sends a tiny probe request (`max_tokens: 1`) to each endpoint and feeds the outcome into the same `Ewma::record()` + eviction path as real traffic. This means a dead backend is evicted **before** the next real request hits it (proactive, not reactive).

**Configuration** (`health.activeProbe` on each model):

| Field | Default | Description |
|---|---|---|
| `interval` | 30s | Time between probe sweeps across all endpoints. |
| `timeout` | 5s | Per-probe connect+read timeout. A timeout is recorded as a failure. |
| `consecutiveFailures` | 2 | Consecutive probe failures before eviction. Independent of real-request failures. |

### 7.2 Eviction

When an endpoint is marked unhealthy (by probe or real-request failure), it is evicted from the active set for a configurable duration.

**Configuration** (`health.eviction`):

| Field | Default | Description |
|---|---|---|
| `duration` | 3s | Base ejection time. Falls back to `Retry-After` header or retry backoff. |
| `consecutiveFailures` | — | Consecutive unhealthy responses before eviction. |
| `restoreHealth` | — | Health score to restore when the endpoint returns from eviction (gradual recovery). |
| `healthThreshold` | — | Health score threshold below which an unhealthy response can evict. |

### 7.3 Reactive eviction (real traffic)

Even without active probing, real-request failures feed the same eviction machinery. An endpoint that returns 5xx or times out has its health score decremented; enough consecutive failures trigger eviction.

### 7.4 Kill-switch

The prober task carries a generation counter. On config reload, the old task's generation becomes stale and the task exits — a new prober starts with the new config. This prevents stale probers from evicting endpoints based on old health policies.

### 7.5 Soft-degrade (G5 key insight)

**NoHealthyEndpoints 503 is unreachable via health eviction on the AI path.** When all endpoints in a backend group are evicted, agentgateway does **soft-degrade**: it reuses evicted endpoints (with reduced health score) rather than returning 503. The 503 fires only when there are **zero endpoints total** (e.g., the backend group is empty or all endpoints have been removed, not just evicted). This means health probing protects against routing to dead endpoints but never causes a hard outage on its own.

---

## 8. The `web_search` server-tool subsystem

Lets a client send a server-side web-search tool in `tools[]` (Anthropic `web_search_20250305` / OpenAI Responses `web_search` / `web_search_preview`) and get a grounded, cited answer — the search runs inline in the sidecar loop, not client-side.

**Architecture:**

1. Client sends request with `tools[]` containing a web search tool type.
2. agentgateway detects the server tool, sets forward headers (`x-ai-ws-llm-url`, `x-ai-ws-llm-auth-header`, `x-ai-ws-llm-auth-value`, `x-ai-ws-config`), and reroutes to the in-pod sidecar (`127.0.0.1:8080`).
3. Sidecar runs the agentic loop: LLM chats → on a search tool_call, runs web_search (Tavily primary, Brave failover) or web_fetch → feeds results back → repeats until the LLM answers.
4. Sidecar returns a chat response tagged with `x-ai-ws-provenance` (rounds + citations + route_type).
5. agentgateway renders the provenance into the client's API shape.

**Spoof-proofing:** internal `x-ai-ws-*` headers are stripped-first on every request before the web-search gate, then set-fresh from config. A client-supplied spoofed `x-ai-ws-llm-url`/auth is dropped.

---

## 9. Deployment topology

### 9.1 Kubernetes — CRDs + controller

The system runs on k8s using Gateway API CRDs: `GatewayClass` → `Gateway` → `HTTPRoute`, with agentgateway-specific CRDs: `AgentgatewayModel`, `AgentgatewayBackend`, `AgentgatewayPolicy`, `AgentgatewayParameter`. The agentgateway controller watches these CRDs and renders them into xDS config that the dataplane consumes.

**Per-route policy attachment:** `AgentgatewayPolicy` with `targetRefs` pointing at an `HTTPRoute` (or `AgentgatewayBackend`) attaches traffic policies (retry, timeout, rate limit, health, transformation) to specific routes or backends.

### 9.2 Standalone mode

For local dev and testing, a single `config.yaml` file defines everything: binds, routes, backends, LLM models, and inline policies. See `examples/*/config.yaml`.

---

## 10. Security model

- **Fail-closed by default.** Policy server down/unreachable/non-200/unparseable → 503. No traffic reaches GPU farms unauthenticated.
- **API keys never stored in plaintext.** Raw key is SHA-256 hashed at the door; only the hash is looked up/cached.
- **Path-tenant binding.** The `path_tenant` (URL path segment) is cross-checked against the key's tenant — stops BYOK cross-tenant cheating.
- **Body/path model consistency guard.** The JSON `body.model` must match the URL path `{model}` segment — prevents naming one model in the body while hitting another model's route.
- **Sidecar SSRF elimination.** Web-search sidecar is localhost-only in-pod — no network path from outside the pod. Internal forward headers are spoof-proofed (strip-all-first, set-fresh).
- **Guardrail directives may carry plaintext secrets** in opaque config blobs → Debug-redacted so they never leak in logs.

---

## 11. Observability

### 11.1 Metrics (Prometheus)

**WS-5 balance & health metrics** (new):

| Metric | Type | Labels | Description |
|---|---|---|---|
| `agw_balance_picks_total` | counter | `backend`, `endpoint`, `result` | Total load-balancer picks. `result` = `selected` / `skipped_evicted` / `skipped_saturated` / `no_endpoints`. |
| `agw_balance_exhausted_total` | counter | `backend` | Increments when the balancer exhausts all endpoints (all evicted/saturated) and falls back to soft-degrade. |
| `agw_health_probe_total` | counter | `backend`, `endpoint`, `result` | Total active health probes. `result` = `success` / `failure` / `timeout`. |
| `agw_health_eviction_total` | counter | `backend`, `endpoint`, `source` | Total endpoint evictions. `source` = `probe` / `real_request`. |

**Policy server metrics:** `ai_gateway_http_requests_total` / `_duration`, `ai_gateway_decisions_total`, `ai_gateway_pipeline_duration_seconds`, `ai_gateway_cache_lookup_*`, `ai_gateway_redis_eval_*`, `ai_gateway_usage_reports_total`, `ai_gateway_tokens_{estimated,actual}_total`, `ai_gateway_token_trueup_delta_total`, saturation gauges.

**Dataplane metrics:** request latency, stream count, upstream latency, retry count, failover count.

### 11.2 Audit log

One structured JSON line per request: tenant, `api_key_hash`, model, estimated/actual tokens, delta, allowed, latency — tied back by `X-Request-Id` to the policy server's tracing spans.

### 11.3 Health endpoints

- Policy server: `GET /health` (liveness, unconditional 200), `GET /ready` (readiness, 200 iff store PING ok).
- Dataplane: readiness/admin endpoint on configured `readinessAddr`.

### 11.4 SLOs

- **AC:** TTFT p50/p95/p99 (SLO e.g. p95 < 300ms), AC rps, queue rejects.
- **AG:** TPOT, completion throughput, end-to-end latency, concurrency.
- **Cache:** prefix hit-rate per farm (the throughput multiplier), eviction pressure.
- **Routing:** failover rate, overflow-to-provider rate (cost signal), farm cooldowns, sticky hit/miss.
- **Health:** probe success rate, eviction rate, soft-degrade rate.

---

## 12. Known issues & gotchas

1. **NoHealthyEndpoints 503 is unreachable via health eviction on the AI path** — soft-degrade reuses evicted endpoints. The 503 fires only at zero endpoints total. Do not expect health probing to cause hard outages; it causes degraded routing instead.
2. **Never retry mid-stream** — avoid duplicate/partial generations. Rely on the failover chain pre-first-token only.
3. **Redis is a shared-state SPOF** — must be HA. agentgateway browns out to per-instance local state if Redis is down (affinity/quotas degrade, no outage); the policy server is fail-closed 503 if its store is down.
4. **Provider overflow = cost tail** — hard-cap with a dedicated TPM budget + alerting; do not let overflow spend run unbounded.
5. **Prompt byte-stability** (client side) is the cheapest, largest hit-rate win. A single varying byte near the front of the prompt nukes the radix-tree prefix match.
6. **One SGLang router per farm** — multiple active routers fragment the cache tree.
7. **Rollout coupling** — a sidecar image bump restarts the dataplane pod. Transient 503 for a few seconds right after a roll (policy server reconnecting) — retries clear it.

---

## 13. Glossary

- **agentgateway** — the dataplane (this repo). A Rust proxy handling LLM routing, balancing, failover, health probing, streaming, and protocol normalization.
- **ai-gateway-plugin-server** — the policy plane. Rust ext-auth + metering server called per request.
- **CRD** — Custom Resource Definition. `AgentgatewayModel`, `AgentgatewayBackend`, `AgentgatewayPolicy`, `AgentgatewayParameter` configure the system in k8s.
- **xDS** — the config protocol the controller uses to push config to the dataplane.
- **SGLang** — the per-farm LLM serving router; `cache_aware` policy, one router/farm, AC/AG worker pools.
- **Dragonfly** — Redis-protocol-compatible multi-threaded store used for the policy server's RL counters.
- **ext-authz** — the integration pattern: the dataplane makes a gRPC authz sub-request per access to the policy server.
- **AC / AG** — autocomplete / agentic traffic classes (opposite profiles, separate worker pools/queues).
- **TTFT / TPOT** — time-to-first-token / time-per-output-token.
- **TPM true-up** — two-phase token-per-minute metering: estimate at `/check`, apply `actual − estimated` delta at `/v1/usage`.
- **Soft-degrade** — when all endpoints are evicted, agentgateway reuses evicted endpoints (reduced health) rather than returning 503.
- **G4** — the active health prober workstream (FR-5.1–5.3).
- **G5** — the soft-degrade analysis workstream.
- **G6 / G7** — sticky affinity and capacity gating workstreams (forward-looking — designed, not yet shipped).

---

*Built 2026-09-14. Verify live tags/state before asserting in production.*
