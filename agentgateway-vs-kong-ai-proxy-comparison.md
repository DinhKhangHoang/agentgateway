# Agentgateway vs. Custom Kong AI-Proxy — Detailed Comparison Report

**Date:** 2026-06-24
**Scope:** Deep feature-by-feature comparison of two AI/LLM gateways:

- **Agentgateway** (`/home/stackops/agentgateway`) — Rust-based, AI-native proxy (Linux Foundation / open source). Built around MCP + A2A + LLM gateway use cases.
- **Custom Kong AI-Proxy** (`/home/stackops/kong`) — Lua/OpenResty fork of Kong Gateway, heavily customized for serving self-hosted LLMs (MiniMax-M2 on GPU farms behind SGLang routers, with online-provider overflow/failover) on VNG Cloud.

---

## 1. Executive Summary

These two projects solve **overlapping but differently-weighted problems**:

| | Agentgateway | Custom Kong AI-Proxy |
|---|---|---|
| **Center of gravity** | Breadth of AI connectivity: agent ⇄ LLM ⇄ tools ⇄ agent (LLM + MCP + A2A), rich governance, guardrails, multi-provider | Depth of self-hosted LLM serving: GPU-farm capacity-aware routing, sticky affinity, failover, TPM gating for one primary model |
| **Language / runtime** | Rust (+ Go controller, Next.js UI) | Lua (LuaJIT on OpenResty/Kong PDK) |
| **Primary deployment** | Standalone or Kubernetes Gateway API | Kong on K8s (KIC), N× pods + Redis HA in front of GPU farms |
| **Best at** | Governance, guardrails, cost accounting, agentic protocols, provider breadth | Inference-aware load balancing, capacity gating, multi-farm failover, MaaS integration |

**Bottom line:** Agentgateway is a broader **agentic governance gateway** with first-class MCP/A2A, guardrails, PII, cost catalogs, and CEL-everywhere policy. The custom Kong is a **specialized inference traffic director** — its standout custom work (multi-farm balancing, capacity strategies, sticky affinity, Prometheus-driven gating) is exactly the layer agentgateway is thinnest on, while agentgateway's guardrails/MCP/A2A/cost layers are exactly what the Kong fork lacks.

They are arguably **complementary** rather than strict substitutes (see §15).

---

## 2. At-a-Glance Capability Matrix

Legend: ✅ first-class · 🟡 partial / via config / generic mechanism · ❌ absent · ➕ custom-built strength

| Capability | Agentgateway | Custom Kong AI-Proxy |
|---|:---:|:---:|
| **Provider breadth** (OpenAI, Anthropic, Gemini, Vertex, Bedrock, Azure, Cohere, DeepSeek, Mistral, HF, Llama…) | ✅ (8 native + Custom framework) | ✅ (14 drivers) |
| Self-hosted / vLLM / VNG Cloud MaaS driver | 🟡 (Custom/OpenAI-compat) | ➕ `aiplatform`, `aiplatform_vllm` (native) |
| OpenAI-compatible unified API | ✅ | ✅ |
| Format conversion (Anthropic↔OpenAI, Bedrock, Gemini, Vertex) | ✅ deep | ✅ deep |
| SSE streaming | ✅ | ✅ |
| WebSocket / Realtime API | ✅ | ❌ |
| Embeddings / Rerank | ✅ | ✅ |
| Token counting (pre-flight tokenizer) | ✅ tiktoken | 🟡 char-heuristic estimate + post-hoc true-up |
| **Weighted / failover / conditional routing** | ✅ (model router + CEL) | ✅ (smooth WRR) |
| **Inference-aware routing** (GPU/KV-cache/queue) | 🟡 K8s Inference Gateway (EPP) extension | ➕ Prometheus capacity gates (delegates KV-cache to SGLang) |
| **Multi-farm / multi-zone balancing with sticky affinity** | 🟡 generic session persistence | ➕ Redis-backed sticky (api-key→farm), least-loaded overflow |
| **Capacity gating** (concurrent / TPM / external metric) | 🟡 rate limits only | ➕ 3 strategies (concurrent, TPM windows, Prometheus) |
| Failover with cooldown / health probes | 🟡 outlier detection | ➕ explicit cooldown SHM + worker-0 health probes |
| Rate limiting RPM | ✅ local + remote (Envoy RLS) | 🟡 stock Kong rate-limit plugin |
| Token-based rate limiting (TPM) | ✅ token-cost RLS | ➕ TPM strategy w/ trailing-window + true-up |
| **Cost accounting** (per-model price catalog, spend) | ✅ rich catalog (input/output/cache/reasoning/audio tiers) | 🟡 cost computed & logged, **not** gated |
| Budget / spend controls | ✅ | 🟡 (recorded only) |
| **Guardrails** (Bedrock/Model Armor/Azure CS/OpenAI moderation/regex/webhook) | ✅ multi-layer | ❌ (delegated to separate plugins/upstream) |
| PII detection & masking | ✅ recognizers (email/SSN/CC/phone/URL) | 🟡 masks media in logs only |
| Streaming guardrails (windowed) | ✅ | ❌ |
| **Auth — frontend** (API key/JWT/OAuth/OIDC/Basic) | ✅ all | 🟡 stock Kong auth plugins |
| **Auth — backend** (AWS SigV4/Azure/GCP SA) | ✅ | ✅ (+ VNG Cloud IAM token exchange ➕) |
| RBAC / fine-grained authz | ✅ CEL allow/deny/require + ext_authz | 🟡 stock Kong ACL/consumer |
| TLS / mTLS | ✅ rich (per-profile, client cert, dynamic CA) | ✅ (Kong core) |
| **Observability — metrics** | ✅ Prometheus + OTel GenAI labels | ➕ very rich `ai_proxy_*` (TTFT/TPOT/e2e, capacity, failover, sticky) |
| Logging / analytics | ✅ OTLP + SQLite/Postgres usage logs | ✅ `serialize-analytics` (tokens/cost/cache) |
| Distributed tracing | ✅ W3C/Jaeger | 🟡 (Kong core tracing) |
| Prompt enrichment / templates (prepend/append) | ✅ | ❌ |
| Prompt caching markers (Anthropic/OpenAI) | ✅ | 🟡 reads cache tokens, doesn't inject markers |
| Semantic / vector caching | 🟡 (analytics tracks cache fields) | 🟡 (analytics tracks vector_db/embeddings) |
| Web search tool injection | ❌ | ➕ `web_search.lua` |
| **MCP gateway** (federation, transports, OAuth, RBAC, guardrails) | ✅ first-class | ❌ |
| **A2A gateway** (agent card discovery, JSON-RPC) | ✅ first-class | ❌ |
| Built-in UI | ✅ (Next.js) | ❌ (Kong Manager / external) |
| Config model | YAML/JSON + xDS + K8s Gateway API | Kong declarative/DB + KIC CRDs |
| Policy language | CEL (routing, authz, transforms, RL, logs) | Lua + schema config |

---

## 3. Architecture & Design Philosophy

### Agentgateway
- **AI-native, multi-protocol.** Treats LLM traffic as one of three first-class flows alongside **MCP** (agent→tool) and **A2A** (agent→agent). The whole proxy is structured around *agentic connectivity*, not just LLM proxying.
- **Policy-as-CEL.** CEL expressions are used pervasively — routing conditions, authorization (allow/deny/require), rate-limit descriptors, request transformations, and log field injection. One consistent expression language across concerns.
- **Rust core** with strong typing per route-type (`Completions`, `Messages`, `Responses`, `Embeddings`, `Realtime`, `Rerank`, `AnthropicTokenCount`, `Passthrough`, `Detect`).
- **Deployment-flexible:** standalone binary, multi-listener, Kubernetes Gateway API with a built-in Go controller, xDS dynamic config.
- Source layout: `crates/agentgateway/src/llm/` (providers, conversion, policy, cost, model_router), `.../mcp/`, `.../a2a/`, `.../http/` (auth, rate limit, authz, tls), `.../telemetry/`.

### Custom Kong AI-Proxy
- **Single-purpose inference director.** Built to run a fleet of N Kong pods in front of GPU farms running one primary model (MiniMax-M2), with online providers as overflow/failover only.
- **9-stage filter pipeline** (`base.lua`): `SETUP → REQ_INTROSPECTION → REQ_TRANSFORMATION → REQ_POST_PROCESSING → RES_INTROSPECTION → RES_TRANSFORMATION → STREAMING → RES_PRE_PROCESSING → RES_POST_PROCESSING`. Each capability is a registerable shared-filter.
- **Clean layer separation (the "golden rule"):** *Kong routes BETWEEN failure domains (farms/providers); SGLang routes WITHIN a farm (KV-cache-aware worker selection).* Kong deliberately does **not** do KV-cache awareness — it delegates that to the per-farm SGLang router.
- **Shared state in Redis HA** (sticky bindings, capacity counters, TPM windows, cooldowns) so all Kong pods agree; falls back to per-pod SHM.
- Source layout: `kong/plugins/ai-proxy/` (handler, schema), `kong/llm/drivers/`, `kong/llm/plugin/shared-filters/`, `kong/llm/plugin/capacity*`, `kong/llm/iam/`.

**Philosophical contrast:** Agentgateway = *broad governance plane for all agentic traffic.* Kong fork = *deep, opinionated traffic-engineering plane for one self-hosted serving topology.*

---

## 4. LLM Provider Support

**Both** expose an OpenAI-compatible unified API and convert to provider-native formats.

| Provider | Agentgateway | Kong fork |
|---|:---:|:---:|
| OpenAI | ✅ (+ Responses, Realtime, embeddings) | ✅ |
| Anthropic | ✅ (Messages, token count) | ✅ |
| Gemini (native) | ✅ | ✅ (`generateContent`) |
| Vertex AI | ✅ (incl. Claude-on-Vertex, rerank) | 🟡 (via gemini driver OpenAI-compat) |
| Bedrock | ✅ (converse, guardrails, rerank) | ✅ |
| Azure OpenAI | ✅ | ✅ |
| Cohere | 🟡 (via Custom) | ✅ |
| DeepSeek | 🟡 (via Custom) | ✅ |
| Mistral | 🟡 (via Custom) | ✅ |
| HuggingFace | 🟡 (via Custom) | ✅ |
| Llama2 (self-host) | 🟡 (via Custom) | ✅ |
| Copilot | ✅ (native) | ❌ |
| **Self-hosted vLLM / SGLang** | 🟡 (Custom / OpenAI-compat) | ➕ `aiplatform_vllm.lua` (purpose-built) |
| **VNG Cloud AI Platform (MaaS)** | ❌ | ➕ `aiplatform.lua` + IAM auth |
| Generic "Custom" extensible provider | ✅ (OpenAI/Anthropic/proprietary formats) | 🟡 (add a new driver) |

**Takeaways:**
- Kong has more *named* drivers out of the box (14), including the bespoke **VNG Cloud MaaS** and **vLLM** drivers tied to your serving stack.
- Agentgateway natively covers the big cloud providers more deeply (Realtime/WebSocket, rerank, Responses API, Copilot) and offers a generic **Custom** provider framework to cover the rest without writing Rust.
- **Gap for agentgateway:** no native VNG Cloud MaaS / IAM driver. You would model it via the Custom OpenAI-compatible provider + a backend auth shim.

---

## 5. Request/Response Transformation & Streaming

| Feature | Agentgateway | Kong fork |
|---|---|---|
| Normalization layer | `llm/conversion/*` (messages, completions, responses, bedrock, gemini, vertex, openai_compat) | per-driver `to_format`/`from_format` + normalize-* filters |
| Anthropic ↔ OpenAI | ✅ | ✅ |
| Bedrock converse-stream | ✅ (`parse/aws_sse.rs`) | ✅ |
| SSE streaming | ✅ (`parse/sse.rs`) | ✅ (`parse-sse-chunk`/`normalize-sse-chunk`) |
| AWS EventStream | ✅ | 🟡 (within bedrock driver) |
| WebSocket / Realtime | ✅ (`parse/websocket.rs`) | ❌ |
| Token counting | ✅ pre-flight tiktoken; Anthropic/Bedrock count endpoints; cache-token conventions | 🟡 char/÷ratio estimate at admission, **true-up** from stream metadata at log phase |
| Image gen / edits / variations | 🟡 | ✅ (`llm/v1/images/*`) |
| Media masking | 🟡 (in logs) | ✅ (`serialize-analytics` masks image/audio/video/file) |

**Notable:** Kong's token handling is *estimate-then-reconcile* (cheap heuristic at admission for capacity reservation, corrected after the response via stream metadata). Agentgateway does *real* tokenization up front (tiktoken), which is more accurate for pre-flight rate limiting and cost, at higher per-request cost. Agentgateway is the only one with **WebSocket/Realtime** proxying.

---

## 6. Routing, Load Balancing & Failover

This is the **most important axis of difference.**

### Agentgateway
- **Model Router** (`llm/model_router.rs`): public/private model visibility, concrete vs virtual models, model aliasing with wildcards (`gpt-4*`), per-model authorization, dynamic `GET /v1/models`.
- **Virtual model strategies:** weighted, failover (primary+fallback), **conditional via CEL** (route on headers/method/request props).
- **Endpoint selection:** two-phase random ("power of two choices") with health scoring to avoid starving degraded endpoints.
- **Outlier detection** (`http/outlierdetection.rs`): parses `Retry-After` / `x-ratelimit-reset` across OpenAI/Anthropic/Cerebras formats; health scoring with failure tracking.

### Custom Kong AI-Proxy
- **Smooth Weighted Round-Robin** (`balance.lua`) with SHM-persisted `current_weight`, atomic locking, uniform-random fallback on contention.
- **Eligibility funnel:** exclude already-tried, in-cooldown, unhealthy, and **over-capacity** targets — *then* pick.
- **Sticky / session affinity** (`balance.lua` Path B + `sticky_store.lua`): `hash(api-key-id [+client-ip])` → bound target stored in **Redis or SHM**, configurable TTL (~1800s), auto-refreshed. Fallback order: **sticky → weighted → least-loaded**. `sticky_by`: `api_key_id` | `header` | `cookie` | `header+ip` | `cookie+ip`.
- **Least-loaded overflow:** when all eligible targets are at capacity, soft-degrade to the lowest `current_count`.
- **Failover** (`failover.lua`): walk primary → fallbacks; on response status in `failover_on` (default `429,503,500,502,504`) mark provider failed with cooldown. **No mid-stream retries** (only gates pre-first-token). Fallback-overlay merging blends partial fallback configs onto the primary request.
- **Health checks** (`health-check.lua`): worker-0 timer loop probing each `(provider,model,upstream)` every 30s with a tiny chat ping (Anthropic-aware payload), 90s status TTL, generation-counter kill-switch.

### Verdict
- **Agentgateway** gives flexible *policy-driven* routing (CEL conditions, weighted/failover, aliasing) and reactive outlier detection — great for cloud-provider mesh routing.
- **Kong fork** gives *traffic-engineering-grade* multi-farm balancing: cross-pod sticky affinity, explicit cooldown state machine, active health probing, and capacity-eligibility filtering. This is **purpose-built for a fixed GPU-farm topology** and is materially more sophisticated for that scenario.
- **Gap for agentgateway:** no cross-instance sticky session→backend affinity, no explicit cooldown SHM/Redis state machine, no active synthetic health probing of LLM endpoints (it relies on passive outlier detection).
- **Gap for Kong:** routing is not expressed as a general policy language; conditional routing on arbitrary request properties is not as flexible as agentgateway's CEL.

---

## 7. Capacity / Inference-Aware Routing

### Custom Kong AI-Proxy — ➕ standout custom capability
A pluggable **capacity system** (`capacity.lua`, `capacity_strategies/`, `capacity_store.lua`) with three strategies, AND-combined per target:
1. **`concurrent`** — max in-flight per target (`current < max`), SHM/Redis counter, 600s lease TTL safety net.
2. **`tpm`** — trailing 60s token windows, separate `input_max`/`output_max` or combined, linear-interpolated decay, **reserve at dispatch / true-up at completion**.
3. **`prometheus`** — poll an external PromQL scalar (e.g. SGLang KV-cache % or queued tokens), gate when `value >= max_value`, fail-open on stale/error. Background poller (`prom-capacity-poller.lua` + `prom_poller.lua`) with dedup keys, generation kill-switch, error metrics.
- **Scope:** `route` (isolated) or `global` (shared) counters.
- **Store backends:** SHM (per-pod, fast) or Redis (cluster-accurate).
- **KV-cache awareness** is deliberately *delegated to the SGLang router* per the golden rule.

### Agentgateway
- **Inference-aware routing** exists but via the **Kubernetes Inference Gateway / Endpoint Picker (EPP) extension** model — routing decisions based on GPU utilization, KV cache, LoRA adapters, and queue depth are described as a feature, leaning on the K8s Inference Gateway ecosystem rather than a built-in Prometheus-polling capacity gate.
- No built-in concurrent-in-flight or TPM-window capacity gate the way Kong has; it uses rate limiting (local + remote RLS) for throttling.

### Verdict
- **Kong fork wins decisively** for *self-managed* GPU-farm saturation control without depending on the K8s Inference Gateway extension. Its concurrent/TPM/Prometheus gates are self-contained and Redis-coordinated across pods.
- **Agentgateway wins** if you're already on Kubernetes Inference Gateway (EPP), where it integrates inference-aware endpoint picking natively into the Gateway API model.

---

## 8. Rate Limiting, Token Budgets & Cost

| | Agentgateway | Kong fork |
|---|---|---|
| RPM limiting | ✅ local (`localratelimit.rs`) + remote Envoy RLS v3 (`remoteratelimit.rs`) | 🟡 stock Kong `rate-limiting` plugin |
| TPM / token-cost limiting | ✅ RLS cost via `llm.totalTokens` or CEL; FailOpen/FailClosed | ➕ TPM capacity strategy (reserve + true-up, in/out axes) |
| Cost catalog | ✅ rich (`llm/cost/`): per-provider/model, context-window tiers, input/output/cache-read/cache-write/reasoning/audio rates, hot reload | 🟡 `input_cost`/`output_cost` per model in schema |
| Cost projection / spend control | ✅ pre-flight projection + budget controls | 🟡 cost computed & logged; **not enforced as a quota** |
| Per-consumer budgets | ✅ | 🟡 per-consumer TPM via capacity scope; cost not gated |

**Verdict:** Agentgateway has the more complete **FinOps/governance** story — a real price catalog with tiered/cache/reasoning/audio rates, pre-flight cost projection, and budget enforcement; plus integration with an external rate-limit service. Kong's strength is **operational throughput control (TPM gating)** tied to capacity, but it *records* cost rather than enforcing spend caps.

---

## 9. Guardrails / Content Safety

### Agentgateway — ✅ major strength
Multi-layer prompt-guard framework (`llm/policy/`) applied on request, response, **and streaming** (windowed evaluation):
- **AWS Bedrock Guardrails** (ApplyGuardrail API)
- **Google Model Armor**
- **Azure Content Safety**
- **OpenAI Moderation API**
- **Regex filters**
- **PII detection/masking** — built-in recognizers: email, US SSN, Canadian SIN, phone, credit card, URL, custom patterns
- **Webhook policies** — custom request/response evaluation with mask/reject actions
- FailOpen/FailClosed modes; streaming guardrails buffer with overlap windows and can inject SSE rejection bodies.

### Custom Kong AI-Proxy — ❌ gap
- **No built-in content filtering** in the ai-proxy code. Guardrails are expected to come from *separate* Kong plugins (e.g. `ai-prompt-guard`, `ai-request-transformer`) or upstream moderation services.
- Only PII handling present is **media masking in analytics logs** (image/audio/video/file → `***MASKED***`), which is about log hygiene, not request-time safety.

**Verdict:** Agentgateway is far ahead on guardrails and content safety. This is one of the largest functional gaps in the Kong fork (by design — it's a routing layer, expecting safety to live elsewhere in the Kong plugin chain).

---

## 10. Auth & Security

| | Agentgateway | Kong fork |
|---|---|---|
| **Frontend auth** | ✅ API key, JWT (RS256/HS256/OIDC), Basic, OAuth2, full OIDC flow w/ sessions | 🟡 stock Kong plugins (key-auth, jwt, oauth2, OIDC enterprise) |
| **Backend auth** | ✅ AWS SigV4 (assume-role), Azure Identity, GCP Service Account, Copilot | ✅ header/query/body, AWS creds, GCP SA, + ➕ **VNG Cloud IAM token exchange** (`iam/accesstoken.lua`) |
| **Authorization / RBAC** | ✅ CEL allow/deny/require (HTTP + TCP), ext_authz (Envoy) | 🟡 stock Kong ACL / consumer groups |
| **TLS / mTLS** | ✅ per-profile cipher/ALPN/version, client-cert verify, dynamic CA (Istio), per-route backend CA | ✅ Kong core TLS/mTLS |

**Verdict:** Agentgateway bundles a richer, self-contained auth+authz stack (especially **CEL RBAC** and **ext_authz**) in the gateway itself. Kong leans on its mature plugin ecosystem for frontend auth/RBAC, but adds a bespoke **VNG Cloud IAM** credential-exchange flow that agentgateway lacks. For backend cloud-provider auth they're comparable.

---

## 11. Observability

### Both are strong, but oriented differently.

**Agentgateway** (`telemetry/`):
- Prometheus metrics with **OTel GenAI semantic labels** (operation_name, system/provider, request_model, response_model).
- Per-token-type metrics (input/output/cache_read/cache_write/reasoning/audio), per-route/model.
- Guardrail metrics (phase, action), cost-catalog lookup status, MCP metrics.
- OTLP logging, usage persistence to **SQLite/Postgres**, CEL-injected log/label fields.
- Distributed tracing (W3C TraceParent, Jaeger), authz decision tracing.

**Custom Kong AI-Proxy** (`prom_metrics.lua`, `serialize-analytics.lua`) — ➕ exceptionally rich *serving* telemetry:
- Latency histograms: **`ai_proxy_ttft_ms`** (time-to-first-token), **`ai_proxy_tpot_latency_ms`** (time-per-output-token), **`ai_proxy_e2e_latency_ms`** (incl. failover hops).
- Routing/health: `failover_total`, `target_marked_failed_total`, `balance_picks_total` (by strategy), `sticky_total` (hit/miss/collision), `balance_exhausted_total`, `target_health`/`target_cooldown` gauges.
- Capacity: `capacity_rejected_total`, `capacity_soft_degrade_total`, `capacity_in_flight`, `capacity_max`, `tpm_reserved_tokens`, `tpm_trueup_delta_tokens`, `prom_poll_error_total`.
- Analytics: structured usage (tokens/cost/time-per-token), meta (provider/model/latency), masked payloads, cache fields (vector_db/embeddings).

**Verdict:** Different lenses. Agentgateway = **GenAI-semantic + governance** observability (cost, guardrail actions, OTel conventions, DB-persisted usage). Kong fork = **serving SRE** observability (TTFT/TPOT, sticky hit rate, capacity rejections, failover reasons) — the metrics you need to operate GPU farms. The Kong fork's routing/capacity metrics have no agentgateway equivalent; agentgateway's cost/guardrail/tracing metrics have no Kong equivalent.

---

## 12. Prompt Manipulation & Caching

| Feature | Agentgateway | Kong fork |
|---|:---:|:---:|
| Prompt enrichment (prepend system / append assistant) | ✅ | ❌ |
| Model aliasing (wildcards) | ✅ | 🟡 (model name mapping in config) |
| Request body transformation | ✅ CEL (`transformation_cel.rs`) | 🟡 (separate transformer plugin) |
| Prompt cache markers (Anthropic/OpenAI cache_control) | ✅ injects markers w/ token thresholds | 🟡 reads cached-token counts, doesn't inject |
| Semantic / vector cache | 🟡 tracks cache fields | 🟡 tracks vector_db/embeddings in analytics |
| **Web search tool injection** | ❌ | ➕ `web_search.lua` (Anthropic tools → upstream) |

**Verdict:** Agentgateway can actively shape prompts (enrichment, cache-control injection, CEL transforms). The Kong fork mostly *observes* prompt/cache metadata but adds a unique **web-search tool injection** capability.

---

## 13. MCP & A2A (Agentic Protocols)

### Agentgateway — ✅ first-class, unique to it
- **MCP gateway** (`mcp/`): SSE, stdio (child process), streamable HTTP, and OpenAPI-discovery transports; tool/prompt/resource/template federation across multiple upstream MCP servers; **CEL RBAC** per resource; **MCP guardrails** (request/response phases); session persistence; OAuth.
- **A2A gateway** (`a2a/`): agent-card discovery (`/.well-known/agent.json`), URL rewriting for gateway-exposed agents, JSON-RPC method inspection.

### Custom Kong AI-Proxy — ❌
- No MCP, no A2A. It is strictly an LLM HTTP proxy.

**Verdict:** This is agentgateway's defining differentiator. If you need to govern agent→tool (MCP) or agent→agent (A2A) traffic, the Kong fork does not address it at all.

---

## 14. Deployment, Config & Ops

| | Agentgateway | Kong fork |
|---|---|---|
| Runtime | Single Rust binary (+ Go controller, Next.js UI) | Kong/OpenResty pods |
| Config | YAML/JSON, xDS, **K8s Gateway API** (HTTPRoute) | Kong declarative/DB, **KIC CRDs** (HTTPRoute) |
| State sharing across instances | xDS / control plane | **Redis HA** (sticky, capacity, cooldown, TPM) + per-pod SHM |
| Built-in UI | ✅ | ❌ (Kong Manager / external) |
| Topology design | General gateway | Documented MaaS plan: L4 LB → N× Kong → Redis → GPU farms (SGLang) + provider overflow; AC vs AG worker-pool split; incident runbooks (`httproute-status-incident-handoff.md`) |
| Protocols | HTTP/1.1, H2, TCP (PROXY proto), WebSocket, gRPC | HTTP (Kong core) |

**Notable Kong ops maturity:** the repo includes a real **capacity-planning / architecture plan** and an **incident handoff** doc (control-plane HTTPRoute status flapping vs. data-plane routing decoupling). This reflects production operational thinking specific to the GPU-farm deployment.

**Notable agentgateway ops:** native **Kubernetes Gateway API** with its own controller, xDS dynamic config, multi-listener binding, and a built-in management UI.

---

## 15. Gap Analysis — What Each Lacks

### What Agentgateway has that the Kong fork lacks
1. **MCP gateway** (tool federation, transports, RBAC, guardrails) — entirely absent in Kong fork.
2. **A2A gateway** (agent-to-agent) — absent in Kong fork.
3. **Guardrails / content safety** (Bedrock/Model Armor/Azure CS/OpenAI moderation/regex/webhook) + **streaming guardrails** — absent in Kong fork.
4. **Built-in PII detection & masking** at request time — Kong only masks media in logs.
5. **Rich cost catalog + spend/budget enforcement** (tiers, cache, reasoning, audio) — Kong records cost but doesn't enforce.
6. **WebSocket / Realtime API** proxying — absent in Kong fork.
7. **CEL everywhere** (routing conditions, authz, RL descriptors, transforms, log fields) — Kong uses Lua/schema config.
8. **Self-contained frontend auth + CEL RBAC + ext_authz** in the gateway core.
9. **Prompt enrichment + cache-control marker injection.**
10. **Built-in UI**, OTel GenAI-semantic telemetry, DB-persisted usage analytics.
11. **Pre-flight real tokenization** (tiktoken) for accurate cost/limits.
12. Native **Copilot** provider; deeper Vertex/Bedrock (rerank, Responses, count endpoints).

### What the Kong fork has that Agentgateway lacks
1. **Self-contained capacity gating** — concurrent / TPM-window / Prometheus strategies, AND-combined, Redis-coordinated, with reserve-and-true-up. (Agentgateway leans on K8s Inference Gateway/EPP instead.)
2. **Cross-instance sticky session→farm affinity** (Redis-backed, configurable key + TTL, hit/miss metrics).
3. **Explicit failover cooldown state machine** + **active synthetic health probing** of LLM endpoints (worker-0 timer, generation kill-switch). Agentgateway relies on passive outlier detection.
4. **Least-loaded overflow** soft-degrade when farms saturate.
5. **VNG Cloud MaaS drivers** (`aiplatform`, `aiplatform_vllm`) + **VNG Cloud IAM** token-exchange auth.
6. **Serving-SRE telemetry** — TTFT, TPOT, sticky hit rate, capacity rejections, failover reasons, balance picks.
7. **Web search tool injection** (`web_search.lua`).
8. **Image generation/edits/variations** route types as first-class.
9. **Documented GPU-farm capacity plan + AC/AG workload segregation + incident runbooks** baked into the repo.
10. Larger set of *named* providers out of the box (14 drivers).

---

## 16. Overlap (Both Do This Well)
- OpenAI-compatible unified API + bidirectional format conversion (Anthropic/Bedrock/Gemini/Azure).
- SSE streaming with token-usage extraction.
- Multi-provider routing with weighted distribution + failover.
- Backend cloud-provider auth (AWS/GCP/Azure).
- Prometheus metrics + structured analytics.
- TLS/mTLS.
- Kubernetes Gateway API / HTTPRoute as the config surface.

---

## 17. Recommendations — When to Use Which

**Use the custom Kong AI-Proxy when:**
- You are operating **self-hosted GPU farms** (SGLang/vLLM) at scale and need cross-pod **capacity gating, sticky affinity, and farm-level failover** without depending on the K8s Inference Gateway extension.
- Your traffic is dominated by **one primary model** with online providers as overflow.
- You need **serving-SRE telemetry** (TTFT/TPOT, capacity rejections) and the **VNG Cloud MaaS/IAM** integration.

**Use Agentgateway when:**
- You need **broad agentic governance**: MCP tool federation, A2A, guardrails, PII, content safety, cost/budget enforcement across many cloud providers.
- You want **policy-driven (CEL)** routing/authz/transformation and a **built-in UI**.
- You need **WebSocket/Realtime**, prompt enrichment, or rich FinOps cost accounting.

**Consider them complementary (layered):**
> Agentgateway as the **north-facing governance & agentic plane** (auth, RBAC, guardrails, MCP/A2A, cost) → forwarding LLM traffic to the **custom Kong AI-Proxy** as the **south-facing inference traffic director** (capacity gating, sticky affinity, multi-farm failover) → SGLang routers → workers.

This layering plays to each system's strength and neutralizes the biggest gaps in both: Kong gains guardrails/MCP/cost from agentgateway in front; agentgateway gains GPU-farm capacity engineering from Kong behind.

---

## 18. Summary Scorecard

| Dimension | Winner | Margin |
|---|---|---|
| Provider breadth | ~Tie (Kong more named, AGW deeper on big-3 + Custom framework) | Slight |
| Format conversion / streaming | Agentgateway (WebSocket/Realtime) | Slight |
| Token/cost accuracy & budgets | **Agentgateway** | Clear |
| Routing flexibility (policy) | **Agentgateway** (CEL) | Clear |
| GPU-farm capacity gating | **Kong fork** | Decisive |
| Sticky affinity / multi-farm failover | **Kong fork** | Decisive |
| Inference-aware routing | Depends (Kong=self-contained Prom gates; AGW=K8s Inference Gateway) | Context |
| Guardrails / content safety | **Agentgateway** | Decisive |
| Auth & RBAC (in-gateway) | **Agentgateway** | Clear |
| Backend cloud auth + MaaS IAM | Kong fork (VNG IAM) | Slight |
| Observability — governance/GenAI | **Agentgateway** | Clear |
| Observability — serving SRE | **Kong fork** | Clear |
| MCP / A2A / agentic | **Agentgateway** | Exclusive |
| Prompt enrichment / caching markers | Agentgateway | Clear |
| Web search injection | Kong fork | Exclusive |
| Built-in UI | Agentgateway | Exclusive |
| Production serving runbooks/plans | Kong fork | Exclusive |

---

### Appendix — Key Source References

**Agentgateway** (`/home/stackops/agentgateway`):
- LLM core: `crates/agentgateway/src/llm/` — `openai.rs`, `anthropic.rs`, `gemini.rs`, `vertex.rs`, `bedrock.rs`, `azure.rs`, `copilot.rs`, `custom.rs`, `model_router.rs`, `mod.rs`
- Conversion: `crates/agentgateway/src/llm/conversion/`
- Policy/guardrails: `crates/agentgateway/src/llm/policy/` (`moderation.rs`, `bedrock_guardrails.rs`, `google_model_armor.rs`, `azure_content_safety.rs`, `streaming_guardrails.rs`, `pii/`, `webhook.rs`)
- Cost: `crates/agentgateway/src/llm/cost/`
- Auth/RL/authz/TLS: `crates/agentgateway/src/http/`
- MCP / A2A: `crates/agentgateway/src/mcp/`, `crates/agentgateway/src/a2a/`
- Telemetry: `crates/agentgateway/src/telemetry/`

**Custom Kong AI-Proxy** (`/home/stackops/kong`):
- Plugin: `kong/plugins/ai-proxy/{handler,schema}.lua`
- Drivers: `kong/llm/drivers/` (incl. `aiplatform.lua`, `aiplatform_vllm.lua`, `shared.lua`)
- Shared filters: `kong/llm/plugin/shared-filters/` (`balance.lua`, `failover.lua`, `health-check.lua`, `capacity_release.lua`, `prom-capacity-poller.lua`, `register-prom-inventory.lua`, `serialize-analytics.lua`, `normalize-*`, `parse-sse-chunk.lua`)
- Capacity: `kong/llm/plugin/capacity.lua`, `capacity_store.lua`, `capacity_strategies/{concurrent,tpm,prometheus}.lua`, `prom_poller.lua`, `sticky_store.lua`
- IAM: `kong/llm/iam/{iam,accesstoken}.lua`; `kong/llm/web_search.lua`
- Metrics: `kong/llm/plugin/prom_metrics.lua`
- Docs: `ai-coding-serving-architecture-plan.md`, `sglang-router-routing-features.md`, `opencode-maas-guide.md`, `httproute-status-incident-handoff.md`, `CLAUDE.md`
