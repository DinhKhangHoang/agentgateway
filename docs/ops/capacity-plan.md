# AI Coding Serving Platform — Capacity Plan

> **Workload:** AI coding (autocomplete + agentic), model **MiniMax-M2** (MoE, ~230B total / ~10B active) on **8×H100** serving nodes.
> **Topology:** L4 LB (internet) → **N× agentgateway** (Redis shared state; routes across GPU farms + online providers, overflow+failover) → **per-farm SGLang router** → **workers**.
> **Config model:** CRDs (`AgentgatewayModel`, `AgentgatewayBackend`, `AgentgatewayPolicy`) in k8s; standalone `config.yaml` for local dev.
> **Estimate model:** per-unit anchors + scaling formulas. **All throughput numbers are order-of-magnitude and MUST be replaced by a benchmark on your hardware (Phase 1).**

---

## 1. Architecture overview

```
                            Internet
                               │
                    ┌──────────▼──────────┐
                    │   L4 Load Balancer  │  TLS passthrough or terminate
                    │  (NLB/LVS/HAProxy)  │  source-IP hash, conn draining
                    └──────────┬──────────┘
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
        ┌──────────────┐ ┌──────────────┐ ┌──────────────┐   agentgateway (dataplane):
        │ agentgateway │ │ agentgateway │ │ agentgateway │   • auth  • rate-limit (RPM) + TPM budgets
        │      #1      │ │      #2      │ │      #N      │   • balancing across farms (sticky/affinity,
        └──────┬───────┘ └──────┬───────┘ └──────┬───────┘     weighted, least-loaded)
               │                │                │            • cross-farm failover · provider overflow
               └───────────────┼────────────────┘            • cost / token accounting
                               ▼
                      ┌──────────────────┐
                      │   Redis (HA)     │  sticky bindings · cooldown ·
                      │  shared state    │  capacity counters · TPM windows
                      └──────────────────┘
                               │ (routing decision: farm vs provider)
        ┌────────────────────┼─────────────────────────┐
        ▼                    ▼                          ▼
 ┌───────────────┐   ┌───────────────┐          ┌──────────────────┐
 │  GPU Farm A   │   │  GPU Farm B   │   ...    │ Online providers │ (OVERFLOW +
 │ ┌───────────┐ │   │ ┌───────────┐ │          │ OpenAI/Anthropic │  FAILOVER only)
 │ │SGLang rtr │ │   │ │SGLang rtr │ │          └──────────────────┘
 │ │ (HA)      │ │   │ │ (HA)      │ │
 │ └─────┬─────┘ │   │ └─────┬─────┘ │
 │  cache_aware  │   │  cache_aware  │
 │ ┌──┐┌──┐┌──┐  │   │ ┌──┐┌──┐┌──┐  │
 │ │W ││W ││W │..│   │ │W ││W ││W │..│   W = 8×H100 node running MiniMax-M2
 │ └──┘└──┘└──┘  │   │ └──┘└──┘└──┘  │
 │ AC pool │ AG pool │  (autocomplete vs agentic worker pools)
 └───────────────┘   └───────────────┘
```

### Layer responsibilities (clean separation)

| Layer | Owns | Does NOT own |
|---|---|---|
| **L4 LB** | TCP/TLS spread across agentgateway, health-out, connection draining | any LLM logic, routing decisions |
| **agentgateway** | AuthN/Z, **per-consumer TPM/token budgets**, cost, **farm-vs-provider routing**, **session-to-farm affinity**, cross-farm failover, **provider overflow**, **active health probing** | KV-cache awareness, worker selection |
| **Redis** | Cross-instance shared state (sticky, cooldown, capacity, TPM) | request path compute |
| **SGLang router** | **KV-cache-aware worker selection**, intra-farm load balance, worker health/CB | auth, quotas, cost, cross-farm decisions |
| **Worker (8×H100)** | MiniMax-M2 inference, prefix KV cache, continuous batching | routing |

The golden rule: **agentgateway routes *between* failure domains (farms/providers); SGLang routes *within* a farm.** Never let one do the other's job.

---

## 2. Request flow

1. Client (IDE/agent) → L4 → an agentgateway instance (any; state is in Redis).
2. agentgateway: **authenticate (API key via ext-auth)** → check consumer **TPM budget** (Redis) → classify traffic (**autocomplete** vs **agentic**, by route/header/model param).
3. agentgateway derives the **session/affinity key = hash(API-key ID [+ client IP, optional])** and selects a **GPU farm**:
   - **Affinity first:** look up `hash(api_key_id[, client_ip]) → farm` sticky binding in Redis (keeps that identity's traffic on the farm whose SGLang cache is warm). Use the **API-key ID** (credential identifier), never the raw secret.
   - **Capacity check** (farm is "saturated" if **any** gate trips → pick another farm, least-loaded, and **rebind** the key):
     1. **Static caps** agentgateway tracks in Redis — **max concurrent streams/farm** and **TPM/farm**. Pre-configured per farm from its benchmarked capacity.
     2. **Dynamic gate** — a **custom Prometheus query** against the GPU farm, e.g. **KV-cache usage %** (`sglang:token_usage`) or **queued token load** (`total_tokens`), with a soft threshold (e.g. usage > ~85%).
     3. **Health / cooldown** — farm unhealthy or in failover cooldown.
   - **Overflow:** if **all** farms are saturated/down → route to an **online provider** (normalized to OpenAI format by the driver).
4. Forward to that farm's **SGLang router** → `cache_aware` picks the worker with the warmest prefix (within the right AC/AG pool) → MiniMax-M2 generates, streams back (SSE passthrough).
5. agentgateway records tokens/cost, trues-up TPM, refreshes the affinity binding TTL.

**Failover chain:** bound farm → other farms (on 429/503/5xx/timeout) → online provider → error. Never retry mid-stream.

---

## 3. Traffic-class split (autocomplete vs agentic)

You chose to serve both. They have **opposite** profiles and must not share a queue (a 200k-token agentic prefill would head-of-line-block autocompletes):

| | **Autocomplete (AC)** | **Agentic (AG)** |
|---|---|---|
| Prompt | small (few–10k tok), big % cached | large (30k–200k tok), multi-turn |
| Output | tens of tokens | hundreds–thousands |
| SLO | **TTFT p95 < ~300 ms** | throughput; TTFT lenient |
| Cache value | high (repeated file context) | very high (stable system+tools+history) |
| Route | `/v1/.../completions` (FIM) | `/v1/chat/completions` |
| Timeout | read ~15–30 s | read ~600 s |
| SGLang | dedicated **AC worker pool**, small batch, low latency | **AG worker pool**, large batch, high throughput |

**Implementation options (pick per scale):**
- **Same farm, separate worker pools** + a SGLang router per pool. Simpler ops.
- **Separate farms** for AC vs AG at large scale (cleanest isolation).

Either way agentgateway maps the two routes to two upstream targets so their queues/timeouts/SLOs are independent.

### Per-route policy via AgentgatewayPolicy

In k8s, attach per-class policies via `AgentgatewayPolicy` CRDs with `targetRefs` pointing at the `HTTPRoute`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayPolicy
metadata:
  name: ac-coding-timeout-retry
spec:
  targetRefs:
  - group: gateway.networking.k8s.io
    kind: HTTPRoute
    name: ac-coding-route
  traffic:
    timeout:
      requestTimeout: 600s
    retry:
      attempts: 3
      codes: [500, 502, 503, 504]
```

In standalone config, use inline `policies:` on the route (see `examples/ops-ac-vs-ag-segregation/config.yaml`).

---

## 4. Tuning by layer

### 4.1 L4 load balancer

| Setting | Recommended | Why |
|---|---|---|
| TLS | terminate **or** passthrough to agentgateway | passthrough if agentgateway needs SNI/mTLS |
| Idle/read timeout | **≥ 600 s** | streams are long-lived; 60 s default cuts agentic generations |
| Response buffering | **off** | must not buffer SSE |
| PROXY protocol | **on** | agentgateway sees real client IP (affinity/limits) |
| Distribution | **source-IP hash** | minor client stickiness; real affinity is in Redis |
| Health check | agentgateway readiness endpoint | drain on rollout |

### 4.2 agentgateway (dataplane)

| Setting | Recommended | Why |
|---|---|---|
| **Affinity (cache lever)** | sticky on **`hash(API-key ID [+ client IP, optional])`**, `sticky_ttl ≈ 1800 s`, in **Redis** | keeps an identity's traffic on the farm whose SGLang cache is warm. Use the key **ID**, not the raw secret. |
| **Balancing** | sticky → weighted → **least-loaded** overflow | across farms; distinct from SGLang's intra-farm balancing |
| **Capacity awareness** | 3 gates: **concurrent** + **tpm** static caps in Redis, **plus** **prometheus** custom query (KV-cache usage %); `capacity_overflow=least_loaded` | don't feed a saturated farm; static caps are hard limits, Prometheus is the live soft gate |
| **Failover** | retry on `[500,502,503,504]` + TCP errors; chain **farm→farm→provider**; cooldown in Redis | survive farm saturation/outage |
| **Rate-limit (RPM)** | per-consumer request cap | abuse/burst protection |
| **TPM / token budgets** | per-consumer token windows in Redis | **only layer that can do token quotas** — SGLang can't |
| **Provider overflow** | external drivers as last-resort fallback + **dedicated TPM cap** | self-host first; bound external cost |
| **Streaming** | SSE passthrough, **minimize transforms**, buffering off | protect TTFT (MiniMax is already OpenAI-compatible) |
| **Retries** | **none mid-stream**; rely on failover chain (pre-first-token only) | avoid duplicate/partial generations |
| **Timeouts** | per traffic class (AC ~15–30 s, AG ~600 s) | see §3 |
| **Health probing** | active probe interval 30s, timeout 5s, consecutiveFailures 2 | proactively evict dead endpoints before real traffic hits them (G4) |
| **Redis** | **HA** (Sentinel/Cluster); brown-out to per-instance local state if down | shared sticky/cooldown/capacity/TPM; SPOF if single |

### 4.3 SGLang router (per farm)

| Flag | Recommended | Why |
|---|---|---|
| `--policy` | `cache_aware` | coding = huge shared prefixes; cache hits dominate |
| `--cache-threshold` | **0.2** | honor big shared coding prefixes even with large unique tails |
| `--balance-abs-threshold` | **64** | keep; raise →128 only with load headroom |
| `--balance-rel-threshold` | **1.5** | keep |
| `--max-tree-size` | **134217728** (128M chars ≈ 32M tok) | raise toward node KV capacity or it under-predicts hits |
| `--eviction-interval` | **120 s** | keep |
| `--retry-max-retries` | **3** | long gens shouldn't be retried aggressively |
| `--cb-failure-threshold` | **5** | eject a sick node faster |
| `--max-concurrent-requests` | **-1** | let continuous batching admit |
| `--queue-size` / `--queue-timeout-secs` | **200 / 30** | AC pool: shorter timeout |
| `--health-check-interval-secs` | **30** | — |
| **Topology** | **one router/farm (HA active-standby)**; separate **AC/AG pools** | multiple active routers fragment the cache tree |

Copy-paste starting command:
```bash
sgl-router \
  --worker-urls <AG workers...> \
  --policy cache_aware \
  --model-path MiniMaxAI/MiniMax-M2 \
  --cache-threshold 0.2 \
  --balance-abs-threshold 64 \
  --balance-rel-threshold 1.5 \
  --max-tree-size 134217728 \
  --eviction-interval 120 \
  --retry-max-retries 3 \
  --cb-failure-threshold 5 \
  --max-concurrent-requests -1 \
  --queue-size 200 --queue-timeout-secs 30 \
  --health-check-interval-secs 30
```

### 4.4 Client / prompt discipline (biggest free win)

| Practice | Do | Why |
|---|---|---|
| Prefix stability | **byte-stable** system prompt + tool schemas (no timestamps, deterministic JSON key order) | one varying byte near the front nukes the whole prefix match |
| Content ordering | **front-load stable** content (system → tools → repo/files → history), volatile last | radix tree matches longest-common-prefix **from the front** |
| Stable identity | reuse the **same API key** per user | agentgateway affinity = `hash(API-key ID)` → same warm farm across turns; rotating the key causes cold starts |

---

## 5. Performance estimate (per-unit + scaling formulas)

> ⚠️ **Anchors below are rough for a ~10B-active MoE on 8×H100 (FP8). Benchmark in Phase 1 and replace.**

### 5.1 Per-worker (one 8×H100 node, MiniMax-M2, FP8)

| Quantity | Symbol | Rough anchor | Notes |
|---|---|---|---|
| Weights footprint | — | ~230 GB FP8 | of 640 GB HBM (8×80) → ~**400 GB for KV+activation** |
| KV per token | `kv_tok` | ~0.1–0.3 MB/tok | architecture-dependent → **benchmark** |
| KV token capacity | `KV` | ~1.3M–4M tokens | `≈ 400GB / kv_tok` |
| Aggregate decode | `D` | **~3,000–6,000 out tok/s** | continuous batching, dozens concurrent |
| Aggregate prefill | `P` | **~20,000–60,000 tok/s** | compute-bound; long contexts cost |
| Concurrent sequences | `C` | **~32–128** | `≈ KV / avg_context`, prefix-sharing raises effective value |

### 5.2 Worked examples (per worker)

**Agentic (AG):** 40k input (≈30k cached prefix + 10k new) · 500 output · 75% prefix hit
- Prefill work ≈ 10k tok → ≈ 0.25 s at `P=40k`. **Decode dominates:** 500 tok.
- Output-bound throughput ≈ `D / out_tok` = `4000 / 500` ≈ **8 AG req/s/worker**, latency ~10 s, concurrency ~80.
- ⇒ **`AG_req_s_per_worker ≈ 5–10`** (output-bound; cache hit cuts prefill, not decode).

**Autocomplete (AC):** 8k input (≈6k cached) · 50 output · TTFT-critical
- Effective prefill ≈ 2k tok → TTFT ~50–150 ms (well under 300 ms target). Decode 50 tok ~0.5–1 s.
- ⇒ **`AC_req_s_per_worker ≈ 50–100`** (short outputs), if the AC pool isn't blocked by AG.

### 5.3 agentgateway instance (dataplane)

| Quantity | Anchor | Notes |
|---|---|---|
| Concurrent SSE streams | **~1,000–4,000 / instance** | bound by long-lived connections + CPU for passthrough |
| Redis ops/request | ~3–5 small ops | sticky + capacity + TPM; Redis does 100k+ ops/s |
| Added latency | single-digit ms | passthrough; rises if you enable heavy transforms (don't) |

L4 and Redis are **not** the bottleneck at sane scale; the binding constraints are **GPU decode throughput** (workers) and **concurrent stream count** (agentgateway).

### 5.4 Scaling formulas

```
# Workers per farm
AG_workers = ceil(peak_concurrent_AG_sessions / (AG_req_s_per_worker × avg_AG_latency_s))
AC_workers = ceil(peak_AC_rps / AC_req_s_per_worker)
workers_per_farm = AG_workers + AC_workers + ceil(0.2 × total)   # +20% headroom/HA

# Farms
N_farms = max(regions/availability_zones needed,
              ceil(total_workers / workers_a_single_router_handles_comfortably))   # blast-radius + HA

# agentgateway instances
N_agw = ceil(peak_concurrent_streams / streams_per_agw) + 1   # +1 for N+1 HA

# Provider overflow capacity = burst above farm capacity you're willing to pay external $ for
```

**Plug-in example** — target *500 concurrent agentic sessions* + *2,000 AC rps*:
- `AG_workers = ceil(500 / (8 × 10)) = ceil(6.25) = 7` (using ~8 req/s, 10 s) → call it **8**.
- `AC_workers = ceil(2000 / 75) = 27` → **28**.
- `workers ≈ (8+28) × 1.2 ≈ 44` per farm region; **2 farms** for HA ⇒ ~22 workers each, or 1 large + 1 standby.
- Concurrent streams ≈ 500 (AG) + short AC ⇒ `N_agw = ceil(~2500/2000)+1 = 3`.
- Redis: 1 HA pair handles this comfortably.

> These collapse to *measured* numbers once Phase 1 benchmarking replaces the anchors.

---

## 6. Failure modes & resilience

| Failure | Detection | Behavior |
|---|---|---|
| Worker dies/slows | SGLang health + circuit breaker | removed from pool; cache tree prunes its tenant; router reroutes |
| Farm saturated | SGLang queue/429; agentgateway capacity poll | agentgateway overflow → another farm → provider |
| Farm down | agentgateway active health prober (G4) | endpoint evicted; cooldown in Redis; route elsewhere |
| agentgateway instance dies | L4 health-out | drained; sessions continue on other instances (state in Redis) |
| Redis down | connection errors | **brown-out**: per-instance local state; affinity/quotas degrade, no outage. Run Redis HA. |
| Policy server down | ext-auth failure | **fail-closed 503**. No traffic reaches GPU farms unauthenticated. |
| Provider overflow cost spike | TPM/budget meter | capped by dedicated provider TPM budget; alert |
| Cache cold (mis-affinity) | low hit-rate metric | self-heals next turns; fix sticky key stability |
| All endpoints evicted | health prober | **soft-degrade**: reuses evicted endpoints (reduced health), no 503 |

---

## 7. Observability & SLOs

**WS-5 balance & health metrics:**

| Metric | Type | Labels | Description |
|---|---|---|---|
| `agw_balance_picks_total` | counter | `backend`, `endpoint`, `result` | Load-balancer picks. `result` = `selected` / `skipped_evicted` / `skipped_saturated` / `no_endpoints`. |
| `agw_balance_exhausted_total` | counter | `backend` | Balancer exhausted all endpoints → soft-degrade fallback. |
| `agw_health_probe_total` | counter | `backend`, `endpoint`, `result` | Active health probes. `result` = `success` / `failure` / `timeout`. |
| `agw_health_eviction_total` | counter | `backend`, `endpoint`, `source` | Endpoint evictions. `source` = `probe` / `real_request`. |

**Define and dashboard:**
- **AC:** TTFT p50/p95/p99 (SLO e.g. p95 < 300 ms), AC rps, queue rejects.
- **AG:** TPOT, completion throughput, end-to-end latency, concurrency.
- **Cache:** prefix hit-rate per farm (the throughput multiplier), `max_tree_size` eviction pressure.
- **Per-worker `total_tokens`** spread (catches the request-count-gate blind spot — token hotspots while request counts look even).
- **Routing:** agentgateway failover rate, **overflow-to-provider rate** (cost signal), farm cooldowns, sticky hit/miss.
- **Health:** probe success rate, eviction rate, soft-degrade rate.
- **Cost:** $/farm vs $/provider; per-consumer token spend.

Sources: SGLang `smg_*` metrics + `/v1/loads`; agentgateway Prometheus metrics; Redis metrics.

---

## 8. Phased rollout

1. **Benchmark (critical).** 1 farm, 1 SGLang router, a few workers, 1 agentgateway, local state. **Measure** `P, D, C, kv_tok`, TTFT/TPOT, and cache hit-rate on real coding traffic. Replace §5 anchors.
2. **Shared state.** Add Redis HA + 2nd agentgateway. Validate sticky affinity, TPM budgets, cost tracking across instances.
3. **Multi-farm.** Add farm B. Validate capacity-aware farm selection, cross-farm failover, cooldown, affinity stability.
4. **Provider overflow.** Wire external providers as last-resort fallback; validate failover chain + spend cap.
5. **Edge + scale.** Add L4, autoscaling (HPA on workers by queue depth/`total_tokens`; agentgateway by stream count). Load-test to SLO; chaos-test each failure in §6.
6. **Tune from data.** Adjust `cache-threshold`, `max-tree-size`, balance thresholds, AC/AG split, capacity thresholds, health probe intervals from real metrics.

---

## 9. Key risks & decisions

1. **Benchmark before trusting capacity** — §5 anchors are placeholders; GPU throughput is the whole capacity model.
2. **Session→farm affinity must be stable** across turns or SGLang cache locality (your main throughput lever) collapses. Stable sticky key + adequate `sticky_ttl`. *(G6 sticky affinity is forward-looking — designed, not yet shipped.)*
3. **Prompt byte-stability** (client side) is the cheapest, largest hit-rate win.
4. **Don't share AC/AG queues** — head-of-line blocking destroys autocomplete TTFT.
5. **One SGLang router per farm** (mesh receive path unwired in v0.3.2). If you must scale routers, consistent-hash sessions or verify mesh.
6. **Request-count imbalance gate is size-blind** — the AC/AG split mitigates it; monitor per-worker `total_tokens`.
7. **Redis is shared-state SPOF** — must be HA; have the local-fallback brown-out tested.
8. **Provider overflow = cost tail** — hard-cap with a dedicated TPM budget + alerting.
9. **Capacity gating** *(G7 is forward-looking — designed, not yet shipped.)* Per-pod capacity awareness via composed predicates will further protect against saturation.

---

*Companion docs: `docs/ops/architecture.md` (full system architecture), `docs/ops/runbook.md` (operations runbook).*
*Estimates are order-of-magnitude pending Phase-1 benchmark on your hardware.*
