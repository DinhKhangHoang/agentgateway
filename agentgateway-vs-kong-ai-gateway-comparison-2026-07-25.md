# Agentgateway vs. Custom Kong AI Gateway — Comparison (2026-07-25)

**Supersedes:** `agentgateway-vs-kong-ai-proxy-comparison.md` (2026-06-24).
That report scoped Kong to `kong/plugins/ai-proxy/` + `kong/llm/` only. This one covers
**all of `/home/stackops/kong/`** — the MCP plugin suite, the guardrail plugin suite, the
Rust ext-auth policy server, and the Go server-tool sidecar — which changes several of its
headline verdicts. Both repos moved ~50 commits since that date.

- **Agentgateway** (`/home/stackops/agentgateway`) — Rust, AI-native proxy (Linux Foundation OSS). LLM + MCP + A2A.
- **Custom Kong AI Gateway** (`/home/stackops/kong`) — Lua/OpenResty Kong fork + a Rust policy server + a Go tool sidecar. Serves self-hosted LLMs on GPU farms (SGLang/vLLM) with online-provider overflow, on VNG Cloud.

---

## 1. Corrections to the 2026-06-24 Report

These are the material errors in the prior doc, with evidence:

| Prior claim | Reality | Evidence |
|---|---|---|
| Kong: **no MCP** (§13, "agentgateway's defining differentiator") | **Wrong — and was wrong at the time.** Kong has a 4-mode MCP gateway plus 3 companion plugins. | `kong/plugins/mcp-proxy/` (added 2026-04-15), `inbound-auth-mcp` (04-19), `policy-enforce-mcp` (04-22), `outbound-auth-mcp` |
| Kong: **no guardrails** (§9, "largest functional gap") | **Wrong.** Four request-guard plugins landed 2026-07-14. | `kong/plugins/{keyword,open-ai,llama-ai,presidio-ai}-guard-request/`, `ai-guard-shared/` |
| Kong: **no PII**, "masks media in logs only" | **Wrong.** Presidio analyzer/anonymizer integration. | `kong/plugins/ai-guard-shared/presidio_ai_guard.lua` (11.7K) |
| Kong: rate limiting = "🟡 stock Kong plugin" | **Wrong.** Purpose-built AI rate limiter: multi-window, per-model limits, Redis, cluster sync. | `kong/plugins/ai-ratelimiting/` (13.7K schema + `policies/`, `clustering/`, `migrations/`) |
| Kong: prompt enrichment "❌" | **Wrong.** | `ai-prompt-decorator` (prepend/append), `ai-prompt-template` |
| Kong: web search = "`web_search.lua`" | **Understated.** Now a Go sidecar with a generic server-tool framework, agentic loop, and multi-backend failover. | `sidecar/internal/{tool,loop,backend,tools/websearch}/` |
| AGW: inference-aware routing "🟡 leans on K8s ecosystem, described as a feature" | **Understated.** In-proxy `InferencePoolRouter` with fail-open and destination override. | `crates/agentgateway/src/http/ext_proc.rs`, `proxy/httpproxy.rs:1818` (`apply_inference_routing`) |
| AGW: "no cross-instance sticky session→backend affinity" | **Partly wrong.** Stateless encrypted session→backend pinning (HTTP + MCP). | `crates/agentgateway/src/http/sessionpersistence.rs` |
| AGW: "budget enforcement ✅" | **Overstated.** Cost is *projected* (`CostProjection`), not capped. Enforcement is via token-type rate limits, not a spend ceiling. | `llm/cost/mod.rs:130 project()`; `RateLimitType: requests\|tokens` + CEL `cost` |
| AGW: providers "8 native + Custom framework" | **Stale.** 13 named presets added 2026-07-24. | `ProviderPreset` enum in `schema/config.json` |
| AGW source paths (`llm/openai.rs`, `llm/conversion/`) | **Stale.** Providers extracted to a standalone crate. | `crates/llm/src/` |

**Net effect:** the prior report's two "decisive / exclusive" wins for agentgateway — MCP and
guardrails — are now **contested**, not exclusive. Agentgateway still leads on both, but on
depth and integration, not on presence.

---

## 2. What Each Project Actually Is Now

### Agentgateway — one binary, three protocols
A single Rust proxy that treats **LLM**, **MCP**, and **A2A** as first-class flows, with CEL as
the universal policy language (routing conditions, authz, rate-limit descriptors, transforms,
log fields, health expressions). Config via YAML/xDS/Kubernetes Gateway API, with a Go
controller, a Next.js UI, and (new) a model-centric `AgentgatewayModel` CRD.

### Custom Kong AI Gateway — a plugin platform, now polyglot
No longer "Lua only" in practice. Three runtimes:
1. **Lua/Kong** — `ai-proxy` (9-stage filter pipeline, 14 drivers, balancing/capacity/failover), `mcp-proxy`, guard plugins, `ai-ratelimiting`, `ai-acl`, `ai-routing`.
2. **Rust ext-auth server** — the `ai-gateway` plugin is a *client* to an external Rust policy server. One `/v1/check` call replaces key-auth + acl + rate-limiting on the route; `/v1/usage` reports actual tokens for TPM true-up. Always-200 contract, fail-closed on any deviation. Pipeline stages: `authn`, `acl`, `ratelimit`, `guardrails`.
3. **Go sidecar** — owns the server-side tool loop (`sidecar/main.go`): `Tool` interface + `Registry`, web-search tool over Tavily/Brave with failover, client-tool interleaving with suspend/lazy-defer, and OpenAI Chat/Responses + Anthropic Messages output shaping.

**The architectural story that changed:** Kong is centralizing governance into the Rust
server and pushing agentic tool-execution into the Go sidecar, leaving Lua as the data-plane
traffic director. That makes it structurally much closer to agentgateway than it was a month ago.

---

## 3. Capability Matrix (whole-repo scope)

Legend: ✅ first-class · 🟡 partial · ❌ absent · ➕ distinctive strength

| Capability | Agentgateway | Custom Kong |
|---|:---:|:---:|
| **LLM proxying** |
| Named providers | ✅ 8 native + **13 presets** + Custom | ✅ 14 drivers |
| Self-hosted vLLM/SGLang | 🟡 `ollama` preset / Custom | ➕ `aiplatform_vllm.lua` purpose-built |
| VNG Cloud MaaS + IAM | ❌ | ➕ `aiplatform.lua` + `llm/iam/` |
| OpenAI-compatible unified API | ✅ | ✅ |
| Format conversion (Anthropic/Bedrock/Gemini/Vertex) | ✅ deep | ✅ deep |
| SSE streaming | ✅ | ✅ |
| WebSocket / Realtime | ✅ | ❌ |
| Embeddings / Rerank | ✅ | ✅ |
| Image gen / edits / variations | 🟡 | ✅ |
| Pre-flight tokenization | ✅ tiktoken | 🟡 heuristic + true-up |
| **MCP** |
| MCP gateway | ✅ | ✅ |
| Transports | ✅ SSE, **stdio**, streamable-HTTP, OpenAPI | 🟡 SSE, streamable-HTTP (no stdio) |
| OpenAPI → MCP tool conversion | ✅ | ✅ (`openapi-import.lua`, TTL refresh) |
| Tool federation / multiplexing | ✅ prefix modes, fanout, merge-stream | ✅ SHM tag registry, merge |
| MCP authz | ✅ **in-process CEL RBAC** | 🟡 **external** auth/policy services |
| MCP guardrails | ✅ | ❌ |
| MCP failover / balancing / health | 🟡 `failureMode` open/closed | ➕ round-robin + cooldown + health-check |
| Per-tool ACL | ✅ CEL | ✅ allow/deny lists |
| Session persistence | ✅ encrypted stateless | ✅ SSE session resolve |
| **A2A** | ✅ agent cards, JSON-RPC, telemetry | ❌ |
| **Guardrails** |
| Request guardrails | ✅ Bedrock/Model Armor/Azure CS/OpenAI mod/regex/webhook | ✅ keyword, OpenAI mod, Llama Guard, Presidio, `ai-prompt-guard` regex |
| Response guardrails | ✅ | 🟡 `ai-response-transformer` only |
| Streaming guardrails | ✅ windowed | ❌ |
| PII detect/mask | ✅ built-in recognizers | ✅ Presidio (analyzer + anonymizer) |
| Centrally-resolved guardrail config | 🟡 per-route policy | ➕ Rust server `guardrails` stage → ctx |
| **Traffic management** |
| Weighted / failover routing | ✅ | ✅ smooth WRR |
| Conditional routing | ✅ CEL | 🟡 Lua/schema |
| Inference-aware (InferencePool/EPP) | ✅ in-proxy | ❌ (delegates to SGLang) |
| Capacity gating (concurrent/TPM/Prometheus) | ❌ | ➕ 3 AND-combined strategies |
| Sticky affinity | ✅ stateless encrypted | ➕ Redis-shared, api-key→farm, + least-loaded overflow |
| Active health probing | ❌ (passive CEL eviction) | ➕ worker-0 synthetic probes |
| Outlier detection / eviction | ✅ CEL + thresholds | ✅ cooldown state machine |
| **Limits & cost** |
| RPM limiting | ✅ local + Envoy RLS | ✅ `ai-ratelimiting` multi-window |
| Token/TPM limiting | ✅ `RateLimitType: tokens` + CEL cost | ➕ TPM capacity (reserve + true-up) + Rust pre-debit |
| Per-model limits | ✅ | ✅ `model_limits` |
| Cost catalog | ➕ tiered: input/output/cache/reasoning/audio | 🟡 flat input/output cost |
| Spend cap enforcement | 🟡 via token RL, no $ ceiling | 🟡 recorded, not gated |
| **Auth** |
| Frontend auth in-gateway | ✅ key/JWT/OAuth2/OIDC/Basic | ✅ Kong plugins + Rust `authn` |
| Backend auth | ✅ SigV4/Azure/GCP/Copilot | ✅ + ➕ VNG IAM exchange |
| Authorization | ✅ CEL allow/deny/require + ext_authz | 🟡 `ai-acl` model lists + external policy svc |
| **Ops** |
| Serving telemetry (TTFT/TPOT/e2e) | 🟡 | ➕ full histograms |
| GenAI-semantic OTel metrics | ✅ + tool-call telemetry | 🟡 |
| Distributed tracing | ✅ | 🟡 Kong core |
| Built-in UI | ✅ | ❌ |
| K8s API | ✅ Gateway API + `AgentgatewayModel` CRD | ✅ KIC CRDs |
| HA state | xDS / hybrid+Postgres | ➕ Redis HA |
| **Server-side tools** | ❌ | ➕ Go sidecar: registry, agentic loop, client-tool interleave, Tavily+Brave failover |

---

## 4. Where the Real Differences Now Live

### 4.1 MCP — presence is tied, philosophy differs

Both ship a genuine MCP gateway. The split is **where policy lives**:

- **Agentgateway** evaluates MCP authorization *in-process* with CEL RBAC (`mcp/rbac.rs`), and applies MCP guardrails (`mcp/guardrails/`) inline. Self-contained, no extra hop.
- **Kong** delegates to *external services* over HTTP: `inbound-auth-mcp` → auth service, `policy-enforce-mcp` → policy service, `outbound-auth-mcp` → per-target credential injection. Each with a 5s timeout, positive-result cache, and `on_error: deny` fail-closed default.

Kong's design fits a multi-tenant SaaS control plane (`x-agentbase-gateway-*` headers,
gateway-id from Host subdomain, per-tenant credential vending). Agentgateway's fits a
self-contained gateway with no external policy dependency.

**Kong is actually ahead on MCP *traffic engineering***: `mcp-proxy` has a balancer, `failover_on`
status list with cooldown, and active health checks for MCP upstreams. Agentgateway's MCP
resilience is a single `failureMode: failOpen|failClosed`.

**Agentgateway is ahead on MCP *breadth***: stdio transport (child-process MCP servers), MCP
guardrails, in-process CEL RBAC, subscriptions, and tool-name prefix strategies.

### 4.2 Guardrails — Kong closed the gap on request, not on response

Kong's four guard plugins all run **request-phase only** — they register
`parse-request` + one guard filter on the `ai-proxy` pipeline at priority 780–800 (before
ai-proxy). Coverage: keyword lists (external service, parallel checks), OpenAI moderation,
Llama Guard, Presidio PII. Plus upstream `ai-prompt-guard` (regex allow/deny, role-aware).

What Kong still lacks: **response guardrails** (only the LLM-based `ai-response-transformer`)
and **streaming guardrails** entirely. Agentgateway has both, including windowed evaluation
over SSE with injected rejection bodies.

One thing Kong does that agentgateway doesn't: `external_config: true` lets the Rust policy
server resolve guardrail config centrally per tenant and hand it to the plugin via
`kong.ctx.shared.ai_gw_guardrails`. Agentgateway guardrail config is static per-route policy.

### 4.3 Capacity gating — still Kong's decisive, uncontested win

Agentgateway has **no** concurrent-in-flight or TPM-window capacity gate. Confirmed: no
`max_concurrent` / `in_flight` concept anywhere in `crates/agentgateway/src/`. Its throttling
is rate limiting (token-bucket local + Envoy RLS).

Kong has three AND-combined strategies (`concurrent`, `tpm`, `prometheus`), Redis- or
SHM-backed, route- or global-scoped, with reserve-at-dispatch / true-up-at-completion and
fail-open on poller staleness. For saturating GPU farms this is not close.

The counterweight: agentgateway has **real in-proxy InferencePool/EPP routing**
(`apply_inference_routing`, `InferencePoolRouter`), which Kong deliberately declines — its
"golden rule" delegates within-farm KV-cache-aware selection to the SGLang router. These are
two coherent answers to the same problem: agentgateway picks endpoints via EPP; Kong gates
admission via capacity and lets SGLang pick.

### 4.4 Server-side tools — Kong's newest exclusive

The Go sidecar is a genuine capability agentgateway has no equivalent for: a tool-agnostic
agentic loop that runs server-side tools between the client and the LLM, suspends on client
tool calls, resumes with lazy-defer, interleaves parallel calls, and shapes output into
OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages — with provider failover
across Tavily and Brave. Agentgateway proxies tool traffic (MCP); it does not *execute* a
tool loop on behalf of the client.

### 4.5 Provider breadth — now agentgateway's edge, reversed

The prior doc gave Kong the edge on named providers (14 vs 8). The `ProviderPreset` enum
added 2026-07-24 flips this: cohere, ollama, baseten, cerebras, deepinfra, deepseek, groq,
huggingface, mistral, openrouter, togetherai, xai, fireworks — 13 maintained presets on top
of the 8 native providers, all first-class rather than hand-rolled Custom configs.

Kong retains the two that matter most for *this* deployment: `aiplatform` (VNG Cloud MaaS)
and `aiplatform_vllm`, plus VNG IAM token exchange. Those remain unavailable in agentgateway.

---

## 5. True Exclusives

### Only agentgateway
1. **A2A gateway** — agent cards, JSON-RPC inspection, response telemetry.
2. **WebSocket / Realtime API** proxying.
3. **Streaming guardrails** + response guardrails.
4. **MCP stdio transport** and **MCP guardrails**.
5. **CEL as a universal policy language** across routing/authz/RL/transform/logging/health.
6. **Tiered cost catalog** (cache-read/cache-write/reasoning/audio rates, context-window tiers, hot reload).
7. **Prompt cache-marker injection** (`PromptCachingConfig`: system/messages/tools, minTokens).
8. **In-proxy InferencePool/EPP** inference-aware endpoint picking.
9. **Built-in UI** + `AgentgatewayModel` CRD + hybrid mode with Postgres.
10. **Pre-flight real tokenization** (tiktoken).

### Only Kong
1. **Capacity gating** — concurrent / TPM / Prometheus, Redis-coordinated, reserve + true-up.
2. **Server-side tool sidecar** — agentic loop, client-tool interleaving, search-provider failover.
3. **Active synthetic health probing** + explicit failover cooldown state machine (for both LLM and MCP upstreams).
4. **Least-loaded overflow** soft-degrade at saturation.
5. **VNG Cloud MaaS drivers + IAM token exchange.**
6. **Serving-SRE telemetry** — TTFT, TPOT, e2e-with-failover, sticky hit/miss/collision, capacity rejections, balance picks.
7. **Centralized tenant policy resolution** via the Rust ext-auth server (authn/acl/ratelimit/guardrail config in one `/check`).
8. **Redis-shared sticky api-key→farm affinity.**
9. **MCP upstream balancing/failover/health-checking.**
10. **Image generation/edit/variation** route types.

---

## 6. Scorecard

| Dimension | Winner | Margin |
|---|---|---|
| Provider breadth | **Agentgateway** (was Kong) | Slight — Kong keeps MaaS/vLLM |
| Format conversion / streaming | Agentgateway (WebSocket/Realtime) | Slight |
| MCP — breadth & policy depth | Agentgateway | Slight (was: exclusive) |
| MCP — upstream resilience | **Kong** | Clear |
| A2A | Agentgateway | Exclusive |
| Guardrails — request | ~Tie | — (was: decisive AGW) |
| Guardrails — response & streaming | **Agentgateway** | Decisive |
| PII | ~Tie (AGW built-in vs Kong Presidio) | — |
| GPU-farm capacity gating | **Kong** | Decisive |
| Sticky affinity / multi-farm failover | **Kong** | Clear |
| Inference-aware routing | **Agentgateway** (in-proxy EPP) | Clear, different philosophy |
| Rate limiting | ~Tie | — (was: clear AGW) |
| Cost accounting | **Agentgateway** | Clear |
| Spend enforcement | Neither | Both record, neither caps |
| Auth & authz | Agentgateway in-gateway; Kong multi-tenant/external | Context |
| Observability — serving SRE | **Kong** | Clear |
| Observability — GenAI/governance | **Agentgateway** | Clear |
| Server-side tool execution | **Kong** | Exclusive |
| Built-in UI / K8s model API | Agentgateway | Exclusive |

---

## 7. Recommendation

The month-old "complementary layering" conclusion is **weaker now**. Kong has grown its own
MCP gateway, guardrails, PII, and AI rate limiting — the exact layers the prior doc proposed
putting agentgateway in front to supply. Stacking both would now duplicate MCP termination,
guardrails, and rate limiting, and add a hop.

Three honest options:

**A. Stay on Kong, close the remaining gaps.** What's genuinely missing is narrower than it
was: response + streaming guardrails, A2A, WebSocket/Realtime, prompt cache-marker
injection, and a tiered cost catalog. The first is the only one with real safety impact —
a response-phase guard filter on the existing `ai-guard-shared` machinery is a contained
piece of work, and streaming guards are the harder follow-on. Everything else on the list is
a feature bet, not a gap. This preserves capacity gating, the sidecar, MaaS/IAM, and the
serving telemetry that took the most effort to build.

**B. Migrate to agentgateway.** Only worth it if A2A, Realtime, or CEL-everywhere policy
become requirements. You would have to rebuild capacity gating (nothing equivalent exists),
the sticky-farm balancer, active health probes, the VNG MaaS/IAM drivers, and the TTFT/TPOT
telemetry — and either port or drop the Go tool sidecar. That is the bulk of the custom work
in the Kong repo.

**C. Split by traffic class, not by layer.** Route *agentic* traffic (MCP federation, A2A,
tool governance) to agentgateway and *inference* traffic (GPU farms, capacity, sticky,
failover) to Kong, as sibling gateways behind one L4 LB rather than stacked. This avoids the
duplication that layering now causes, at the cost of two control planes.

Given that Kong already carries the MaaS-specific work — capacity, IAM, vLLM drivers, farm
topology, runbooks — **A is the default**, with C worth considering only if A2A or heavy MCP
federation becomes a real requirement.

---

## 8. Deep Dive — Failover, Balancing, Health Checking

Read from source, not summaries. Kong: `balance.lua` (521 lines), `failover.lua` (179),
`health-check.lua` (369), `target_pool.lua`, plus a **separate trio** under `mcp-proxy/filters/`.
Agentgateway: `types/loadbalancer.rs`, `http/health.rs`, `http/outlierdetection.rs`,
`http/retry/mod.rs`, `llm/model_router.rs`, `types/local.rs`.

### 8.1 Load balancing algorithm

| | Agentgateway | Kong |
|---|---|---|
| Algorithm | **P2C** (power of two choices), sampling *with replacement* | **Smooth WRR** (port of nginx `upstream_round_robin.c`) |
| Weighting | `WeightedIndex` sampling by endpoint `capacity`; `Uniform` sampler when all caps are 1; `Drained` state when all are 0 | `weight` per target (default 100), `current_weight` in SHM, 3600s TTL |
| Tie-break / scoring | `score = health / (1 + latency_penalty)` where `latency_penalty = ewma_latency × (1 + pending × 0.1)` | none — pure weight rotation |
| Concurrency awareness | ✅ in-flight `pending_requests` folded into score | 🟡 only via capacity strategies / `least_loaded` overflow |
| Latency awareness | ✅ EWMA per endpoint | ❌ (measured for metrics, not for picking) |
| Locality awareness | ✅ `LocalityRanker` buckets, scanned in order | ❌ |
| Concurrency control | lock-free (`ArcSwap` + atomics; `action_mutex` only for mutators) | `resty.lock` per pick (50 ms timeout, 0.5 s exptime), **uniform-random fallback on lock failure** |
| Explicit destination override | ✅ `select_override` — bypasses bucketing *and* health | ❌ |

**The substantive difference:** agentgateway picks on *live signal* (EWMA latency + in-flight
depth + health score). Kong picks on *static weight*, then subtracts targets that fail a
binary eligibility test. Kong's approach is more predictable and easier to reason about for a
fixed farm topology; agentgateway's naturally sheds load from a slow-but-up backend, which
Kong cannot do — a farm that is degraded but not failing keeps receiving its full weight share
until a capacity gate or health probe trips.

Kong's counterweight is the **eligibility funnel**, which agentgateway has no equivalent for:
`not tried this request → not in cooldown → not probed-unhealthy → passes capacity check`,
evaluated before the pick rather than as a score adjustment.

### 8.2 Sticky / session affinity

Both have it; the mechanisms are not comparable.

- **Agentgateway** (`sessionpersistence.rs`): the backend `SocketAddr` is serialized into an
  **encrypted session token**. Stateless — any replica decrypts and honors the pin with no
  shared store. Two variants: `HTTPSessionState` (one backend) and `MCPSessionState` (per-target
  session list). Pins to a *specific endpoint*.
- **Kong** (`balance.lua` + `sticky_store.lua`): two paths driven by `sticky_ttl`.
  - `sticky_ttl == 0` → **Path A**, stateless weighted hash-mod on `sha1(client_id)`. The code
    comments that consistent hashing (`resty.chash`) was intended but is not a dependency, so
    hash-mod is used — meaning **any change to the eligible set remaps every client**, not just
    the affected share.
  - `sticky_ttl > 0` (default **300 s**) → **Path B**, a stored binding with sliding-TTL
    refresh; on miss/expiry/degradation it re-picks via `weighted_random` and rebinds.
  - `sticky_by`: `consumer` | `header` | `cookie` | `ip`, plus composed `consumer+ip`,
    `header+ip`, `cookie+ip`. Composed keys join `part=value` with `|` so `consumer=X|ip=Y`
    can't collide with `cookie=X|ip=Y`. A missing component degrades silently.
  - `client_id` is sha1-hashed before entering SHM keys (keeps API keys out of key strings).

Correction to the prior doc: `sticky_by` is `consumer`, not `api_key_id`, and the TTL default
is **300 s**, not 1800.

### 8.3 Failover

**Kong — two-layer, explicit state machine.** `balance.lua` owns the active pool
(`targets[]`); `failover.lua` owns the standby pool (`fallbacks[]`) and only engages when
balance didn't pick (`ngx.ctx.ai_balance_picked` gate). Pool exhaustion sets
`ai_balance_exhausted`, which hands off to failover to walk fallbacks in declaration order.

- Targets are benched in a shared cooldown keyspace (`ai_fo:*`), written by **both** filters —
  one source of truth for "this target is sick."
- Marked on: (a) response status in `failover_on` (default `429,500,502,503,504`) at
  `RES_INTROSPECTION`; (b) the previous attempt's target on retry; (c)
  `ngx.balancer.get_last_failure()` on the balancer retry path.
- Cooldown default **180 s** (LLM) / **60 s** (MCP). `failover_cooldown = 0` is special-cased —
  a naive `shm:set(key, 1, 0)` means *never expire*, the exact opposite of intent, so the write
  is skipped entirely.
- `build_overlay_conf` deep-copies the primary and overlays only what the fallback specifies —
  partial fallback configs inherit timeouts/auth/options.
- Last resort: everything in cooldown → retry the primary anyway rather than fail.
- Notable constraint documented in the source: nginx `proxy_next_upstream` **cannot** drive
  this, because ai-proxy buffers through an `ngx.location.capture` subrequest which does not
  retry on HTTP status. Retry is therefore Lua-driven.

**Agentgateway — priority groups + generic retry.** Virtual models support three strategies
(`model_router.rs` / `types/local.rs:460+`):
- `weighted` — `choose_weighted`.
- `failover` — **targets grouped by `priority`** (lower tried first). *Within* a priority level
  the best provider is chosen by the composite health+latency score; when every model in a
  group is degraded, traffic moves to the next group.
- `conditional` — CEL `when` expressions evaluated in order, with an optional unconditional
  final fallback.

Underneath, `http/retry/mod.rs` gives `attempts`, `backoff`, retryable `codes`, a CEL
`condition` on the response, and a CEL `precondition` on the request that skips body buffering
when the request is known non-retriable (streaming/websockets). A failed priority group is
evicted before the next attempt.

**Assessment:** agentgateway's priority-group model is the more elegant abstraction — graded
degradation with in-group scoring, rather than Kong's binary active-pool / standby-pool split.
Kong's is more *operationally explicit*: you can inspect exactly which target is benched and
for how long, and the cooldown survives the request that caused it.

### 8.4 Health checking — the clearest split

**Kong: active synthetic probing.** This has no agentgateway equivalent.

- Worker-0 only (avoids N-worker fan-out), self-rescheduling `ngx.timer.at` loop.
- Interval **60 s**, status TTL 180 s (3× interval), per-probe timeout 55 s.
  *(Correction to the prior doc, which said 30 s / 90 s — those are the MCP probe's values.)*
- Probe is a **real inference call**: `{"messages":[{"role":"user","content":"ping"}], "max_tokens":10}`.
  Anthropic gets `max_tokens` only — sending `max_completion_tokens` would 400 and *mask a
  genuine 429*. `max_tokens` is deliberately 10 rather than 1 so that providers reserving
  `max_tokens` against a TPM budget at admission trip 429 near the real boundary.
- Unhealthy = **5xx or 429**. Other 4xx (auth/validation) count as healthy — they still prove
  reachability.
- Per-driver `do_health_check` hook supplies URL + headers; providers without it are skipped.
- **Generation-counter kill switch** (`ai_hc_gen:*`): bumped on reconfigure, so a running loop
  self-terminates on its next tick.
- **Config fingerprint** (`ai_hc_fp:*`): if probe-relevant fields are unchanged, the timer is
  left alone — prevents a timer storm when KIC updates unrelated routes.
- Active-key diffing stops probes and cleans SHM for targets removed from config.
- Probes the primary, every fallback, and every pool target, inheriting primary auth.
- Unknown status (no probe yet) is treated as **healthy**, so the first request isn't blocked.

**Agentgateway: passive eviction only.** Nothing dials a backend on a timer.

- `http/health.rs`: CEL `unhealthyExpression` (default = any 5xx, non-zero gRPC status, or
  connection failure), with `eviction.duration`, `consecutiveFailures`, `healthThreshold`,
  `restoreHealth` (scores validated to 0.0–1.0).
- **Multiplicative backoff**, deliberately uncapped: `duration × (times_ejected + 1)`. The
  comment explains the reasoning — if everything ends up evicted the LB falls back to serving
  from the rejected pool anyway, so an arbitrary cap would only distribute load unevenly across
  equally-degraded backends.
- Default eviction 3 s, or `Retry-After` / retry-backoff when no duration is set.
- `outlierdetection.rs` is narrower than its name suggests — it is essentially one function,
  `retry_after()`, parsing `Retry-After` (seconds or HTTP-date), `x-ratelimit-reset-ms`,
  `x-ratelimit-reset` (seconds vs. epoch disambiguated by a 30-day threshold), plus OpenAI's
  `x-ratelimit-reset-requests/tokens` and Cerebras' day/minute variants. It feeds eviction
  duration; it does not itself detect outliers.
- The LB never hard-fails: `select_fallback` will serve from the **rejected** (evicted) pool
  when no active endpoint is viable, skipping only operator-drained buckets.

**Consequence:** agentgateway discovers a dead backend only when a real request hits it — the
first request after an outage pays the failure. Kong knows within ≤60 s without customer
traffic, which matters when a farm drains overnight and the first morning request would
otherwise land on it. Conversely Kong pays a standing probe cost against every provider
(including paid ones) and, at 60 s, can be up to a minute stale.

### 8.5 The multi-pod caveat — Kong's state is less shared than it looks

The prior report described "Redis HA (sticky, capacity, cooldown, TPM)". Verified against
source, that is **only half right**:

| State | Backend | Cross-pod? |
|---|---|---|
| Sticky bindings | `sticky_store` — `shm` \| `redis`, **default `shm`** | Only if `balancer.store = redis` |
| Capacity counters / TPM | `capacity_store` — `shm` \| `redis`, **default `shm`** | Only if `balancer.capacity_store = redis` |
| **Failover cooldown** (`target_pool`) | `ngx.shared.kong_ai_proxy_cache` — **SHM only, no Redis option** | ❌ never |
| **Health probe status** | SHM only | ❌ never |
| Smooth-WRR `current_weight` | SHM only | ❌ never |

So in an N-pod deployment: every pod probes every provider independently (N× probe traffic), a
target benched on pod A still receives traffic from pod B until B independently observes a
failure, and WRR distribution is per-pod rather than global. Both stores that *can* use Redis
default to `shm`, so this is also the out-of-the-box behavior unless explicitly configured.

This is defensible — each pod converges on its own, and per-pod state avoids a Redis dependency
in the hot path — but it is materially weaker than "Redis-coordinated failover" and should not
be assumed. Agentgateway has the same class of limitation (eviction state is per-instance), so
neither is globally coordinated; the difference is that Kong's Redis option creates the
impression of coordination that the cooldown and health paths do not actually have.

### 8.6 The MCP layer is a second, independent stack

Kong's `mcp-proxy` reimplements all three concerns rather than reusing the LLM ones, and the
implementations are weaker:

| | LLM (`kong/llm/`) | MCP (`mcp-proxy/filters/`) |
|---|---|---|
| Balancing | Smooth WRR, weighted | **Plain round-robin** — `shm:incr % #healthy` |
| Sticky | 2 paths, 7 `sticky_by` modes | none |
| Capacity gating | 3 strategies | none |
| Cooldown default | 180 s | 60 s |
| Probe | real chat completion, `max_tokens: 10` | JSON-RPC `ping` |
| Probe interval | 60 s | 30 s |
| Unhealthy criteria | 5xx **or 429** | **5xx only** — 429 counts as healthy |
| No-healthy behavior | exhausted → failover → last-resort primary | warn, then use **all** targets; 503 only if the list is empty |

**Two defects worth filing:**

1. **`mcp-proxy` accepts a per-target `weight` and never reads it.** `schema.lua:62` defines
   `weight` (default 100); `grep -rn weight kong/plugins/mcp-proxy/` returns that line and
   nothing else. `balancer.lua` picks with `((counter - 1) % #healthy) + 1` — pure round-robin.
   Any weighted MCP config is silently ignored, and `algorithm` is `one_of {"round-robin"}`, so
   there is no way to opt into weighting either.
2. **MCP health-check treats 429 as healthy** (`res.status < 500`), while the LLM probe
   explicitly treats 429 as unhealthy — and the LLM code documents *why* (a rate-limited
   upstream can't serve). A rate-limited MCP server stays in rotation.

Agentgateway's MCP path, by contrast, has no dedicated balancer/probe at all — just
`failureMode: failOpen | failClosed` over the target set, with session pinning via
`MCPSessionState`. So Kong wins on MCP resilience *by having any at all*, but the
implementation is a first pass, not the LLM stack's maturity.

### 8.7 Summary

| Concern | Winner | Why |
|---|---|---|
| Balancing algorithm sophistication | **Agentgateway** | P2C + EWMA latency + in-flight depth + locality vs. static weights |
| Balancing predictability / control | **Kong** | Explicit eligibility funnel, capacity-aware, `least_loaded` overflow |
| Sticky affinity | **Agentgateway** | Stateless encrypted pin needs no shared store; Kong's Path A is hash-mod, not consistent hashing |
| Sticky flexibility | **Kong** | 7 `sticky_by` modes incl. composed forms |
| Failover model | **Agentgateway** | Priority groups with in-group scoring vs. binary active/standby |
| Failover observability & control | **Kong** | Inspectable cooldown keyspace, per-target bench state, overlay-merged fallback configs |
| Health checking | **Kong** | Active probing exists; agentgateway has none |
| Health-check engineering quality | **Kong** | Generation kill-switch, config fingerprinting, dialect-aware probe bodies, TPM-aware `max_tokens` |
| Eviction/backoff modeling | **Agentgateway** | CEL predicates, health scores, consecutive-failure thresholds, multiplicative backoff |
| Cross-pod state coordination | **Neither** | Kong: cooldown + health are SHM-only; agentgateway: per-instance |
| MCP-layer resilience | **Kong** (with caveats) | Has balancer/failover/probe at all — but weight is ignored and 429 mishandled |

**The one gap that matters most for your deployment:** agentgateway has no active health
probing. For GPU farms that drain, restart, or go dark outside traffic hours, Kong's 60 s
synthetic probe is the difference between discovering an outage proactively and discovering it
via a customer request. If agentgateway were ever adopted, that is the first thing to rebuild —
and there is no extension point for it short of an external controller flipping endpoint
capacity to 0 (which the `Drained` sampler state would at least honor cleanly).

---

## Appendix — Source References

**Agentgateway:**
- Providers: `crates/llm/src/` (`openai.rs`, `anthropic.rs`, `bedrock.rs`, `vertex.rs`, `azure.rs`, `copilot.rs`, `gemini.rs`, `custom.rs`); presets in `ProviderPreset` (`schema/config.json`)
- Conversion / types: `crates/llm/src/conversion/`, `crates/llm/src/types/`
- Guardrails: `crates/agentgateway/src/llm/policy/` (`streaming_guardrails.rs`, `bedrock_guardrails.rs`, `google_model_armor.rs`, `azure_content_safety.rs`, `moderation.rs`, `webhook.rs`, `pii/`)
- Cost: `crates/agentgateway/src/llm/cost/` (`catalog.rs`, `refresh.rs`)
- Routing/resilience: `http/sessionpersistence.rs`, `http/health.rs`, `http/outlierdetection.rs`, `http/ext_proc.rs`, `proxy/httpproxy.rs:1818`
- MCP / A2A: `crates/agentgateway/src/mcp/` (`rbac.rs`, `auth.rs`, `guardrails/`, `streamablehttp.rs`), `src/a2a/`
- K8s: `controller/install/helm/agentgateway-crds/templates/agentgateway.dev_agentgatewaymodels.yaml`

**Custom Kong:**
- LLM proxy: `kong/plugins/ai-proxy/`, `kong/llm/drivers/`, `kong/llm/plugin/shared-filters/`
- Capacity: `kong/llm/plugin/capacity.lua`, `capacity_strategies/{concurrent,tpm,prometheus}.lua`, `capacity_store.lua`, `sticky_store.lua`, `prom_poller.lua`
- MCP: `kong/plugins/mcp-proxy/` (+ `filters/`, `tool-registry.lua`), `inbound-auth-mcp/`, `outbound-auth-mcp/`, `policy-enforce-mcp/`, `kong/mcp/`
- Guardrails: `kong/plugins/ai-guard-shared/` (`presidio_ai_guard.lua`, `open_ai_guard.lua`, `llama_ai_guard.lua`, `keyword_guard.lua`), `{keyword,open-ai,llama-ai,presidio-ai}-guard-request/`, `ai-prompt-guard/`
- Policy/limits: `kong/plugins/ai-gateway/` (Rust ext-auth client), `ai-ratelimiting/`, `ai-acl/`, `ai-routing/`
- Server tools: `sidecar/` (`main.go`, `internal/tool/`, `internal/loop/`, `internal/backend/{tavily,brave,failover}.go`, `internal/tools/websearch/`)
- IAM / metrics: `kong/llm/iam/`, `kong/llm/plugin/prom_metrics.lua`
