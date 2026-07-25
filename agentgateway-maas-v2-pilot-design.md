# Agentgateway MaaS-v2 Pilot — Design Spec

**Date:** 2026-07-25
**Status:** Draft for review
**Goal:** Stand up an agentgateway-based gateway in a new namespace that accepts the same client
requests as the Kong AI gateway in `user-11377-maas-v2`, covering one route of every distinct
shape, so the migration path can be evaluated on real traffic.

**Related:** `agentgateway-vs-kong-ai-gateway-comparison-2026-07-25.md` (capability comparison, §8 failover/balancing/health deep dive).

---

## 1. Decisions Taken

| Decision | Choice | Rationale |
|---|---|---|
| Placement | New namespace `user-11377-maas-v2-agw` | Kong untouched; clean rollback; enables side-by-side comparison |
| Scope | Representative thin slice (5 models) | Every *shape* and every *blocker* exercised without porting 145 configs |
| Governance | Reuse the Rust ai-gateway server behind a thin ext_authz adapter | Preserves the multi-tenant control plane; zero change to the production server |
| Web search | Deferred, documented as a gap | No agentgateway extension point; would exceed the rest of the pilot combined |
| Exposure | LoadBalancer + nip.io hostname, TLS on 443 | Mirrors the Kong dev setup; externally reachable and directly comparable |
| Routing | **Approach B** — legacy path regex → `URLRewrite` → model matched from `body.model` | Clients unchanged, so real/replayed traffic can be pointed at the pilot |

### 1.1 Refinement adopted during design

**The pilot gets its own `ai-gateway-plugin-server` instance**, not a shared one with production.

Kong's plugin pre-debits a token estimate at `/v1/check` and corrects it at `/v1/usage` in the log
phase. Agentgateway has no log-phase webhook, so the pilot cannot close that loop in Phase 1
(see §6). If the pilot called the production server, it would pre-debit real tenant budgets that
were never trued up, corrupting production accounting. Running a separate instance (same image,
own config and database) isolates this completely and still leaves the production server
unmodified.

---

## 2. Architecture

```
                        LoadBalancer  (nip.io, :443 TLS)
                                  │
                    Gateway "maas-v2-agw"  (gatewayClass: agentgateway)
                                  │
        ┌─────────────────────────┼──────────────────────────┐
        │                         │                          │
    3 HTTPRoutes           7 AgentgatewayModel        AgentgatewayPolicy
   legacy path regex        match on body.model        extAuth / telemetry
   → URLRewrite             → provider backend
                                  │
                                  ▼
                        upstream LLM providers

  Supporting workloads in-namespace:
    extauthz-adapter            (new, thin)  ──►  ai-gateway-plugin-server (pilot copy) ──► own DB
    agentgateway controller + CRDs           (cluster-scoped, installed once)
```

### 2.1 Components

| Component | Type | Notes |
|---|---|---|
| agentgateway CRDs | Cluster-scoped | `AgentgatewayModel`, `AgentgatewayBackend`, `AgentgatewayPolicy`, `AgentgatewayParameters`. Helm chart `agentgateway-crds`. |
| agentgateway controller | Deployment (own ns, e.g. `agentgateway-system`) | Helm chart `agentgateway`. Manages `GatewayClass: agentgateway`. Set `discoveryNamespaceSelectors` to include the pilot namespace. |
| Gateway `maas-v2-agw` | `gateway.networking.k8s.io/v1` | One HTTPS listener :443. Controller provisions the data-plane Deployment + LoadBalancer Service. |
| HTTPRoutes | 3 | One per API surface, not per model: `/v1/chat/completions`, `/v1/messages`, `:generateContent`. A full migration adds embeddings, images, and rerank — still ~6 total, versus Kong's 128. |
| AgentgatewayModels | 7 (5 public, 2 internal) | Model → provider mapping. |
| AgentgatewayPolicy | 1–2 | `traffic.extAuth`, `frontend.metrics`, `frontend.tracing`, `frontend.accessLog`. |
| `extauthz-adapter` | Deployment + Service | New. ~50 lines. See §5.2. |
| `ai-gateway-plugin-server` | Deployment + Service | Existing image `ai-gateway-plugin-server:maas-v2`, pilot config + own DB. |

---

## 3. Request Flow

1. Client sends `POST /maas/user-11377/deepseek/deepseek-v4-pro/v1/chat/completions`
   with body `{"model": "deepseek-v4-pro", ...}` — byte-identical to what Kong accepts today.
2. **HTTPRoute** matches the legacy regex and applies `URLRewrite` to `/v1/chat/completions`.
3. **`traffic.extAuth`** calls the adapter, which calls the Rust server's `/v1/check`.
   On allow, `X-Tenant-ID` is copied onto the upstream request via `includeResponseHeaders`.
4. **Request guardrails** (`promptGuard.request`) evaluate the prompt.
5. **Model router** reads `body.model`, resolves the matching `AgentgatewayModel`, and selects the
   provider backend (or a virtual model's target).
6. Response streams back. **Response and streaming guardrails** apply (`promptGuard.response`,
   `promptGuard.streaming: Enabled`).

### 3.1 Accepted behavior change

Kong validates that `body.model` equals the model segment in the URL path and returns 400 on
mismatch. Rewriting the path away removes that check: agentgateway routes purely on `body.model`,
so a request with a mismatched path would succeed against the body's model rather than erroring.

This is **accepted for the pilot**. If exact parity is required, it can be restored with a CEL
condition on the route comparing the original path segment to `body.model`. Recorded as an open
item in §9.

---

## 4. Model Configuration

Five models, chosen so that each exercises a distinct shape and each known blocker appears at
least once.

| # | Model (as named in Kong today) | `provider` | Shape proven |
|---|---|---|---|
| 1 | `byok-11374-openai-generative-model` | `OpenAI` + `baseURL` | The 108-route majority case (BYOK, custom base URL) |
| 2 | `gemini-2.5-flash` | `Gemini` | Native provider; `:generateContent` path form |
| 3 | `claude-sonnet-4-0` | `Anthropic` | The `/v1/messages` API surface |
| 4 | `deepseek-v4-pro` | virtual, `weighted` | The 99:1 traffic split |
| 5 | `qwen2-0.5b` (aiplatform endpoint `me-8e5e98cf-…`) | `Custom` + IAM shim | The VNG MaaS / IAM blocker |

### 4.1 The weighted split

Kong expresses this as `balancer.targets[]` with weights 99 and 1. Agentgateway expresses it as
three resources:

- `deepseek-v4-pro-direct` — `visibility: Internal`, provider `Deepseek`/`OpenAI` + `baseURL` for `api.deepseek.com`
- `deepseek-v4-pro-dashscope` — `visibility: Internal`, `baseURL` for the dashscope compatible-mode endpoint
- `deepseek-v4-pro` — `visibility: Public`, `virtualModel.weighted` with `modelRef` targets at weights 99 and 1

`Internal` visibility means the two concrete backends cannot be requested directly by clients —
only the virtual model is addressable. This is cleaner than Kong's equivalent, where every target
is implicitly reachable through the same plugin.

### 4.2 Secrets

Kong stores provider API keys, VNG IAM secret keys, and the Redis password **inline in plaintext**
in the `KongPlugin` specs. The pilot MUST use Kubernetes `Secret` references for all credentials
via `AgentgatewayModel.policies.auth`. No credential is to appear literally in any manifest
committed to git.

---

## 5. Governance

### 5.1 What maps directly

| Kong | Agentgateway |
|---|---|
| `ai-gateway` plugin → Rust `/v1/check` | `traffic.extAuth` (HTTP protocol) → adapter → Rust server |
| Tenant header injection | `extAuth.http.includeResponseHeaders: [X-Tenant-ID]` |
| `pipeline_stages: [authn, acl, ratelimit]` | Unchanged — still resolved server-side by the Rust server |
| Fail-closed on auth failure | `extAuth.failureMode: deny` (the default) |

Agentgateway's HTTP ext_authz supports CEL expressions for `path`, `body`, and
`addRequestHeaders`, so the `/v1/check` request payload can largely be constructed in config.

### 5.2 Why the adapter is required

Agentgateway's HTTP ext_authz derives the allow/deny decision from the **authorization service's
HTTP status code**. The Rust server deliberately implements an *always-200* contract, with the real
decision in the JSON body (`allowed`, `status_code`, `reason`, `tenant`, `rate_limits`).

Wired directly, agentgateway would read HTTP 200 and allow every request — a silent fail-open on
the authorization path. The adapter exists solely to translate:

- `200 {allowed: true, tenant: T, rate_limits: …}` → `200` + `X-Tenant-ID: T` + `X-RateLimit-*` headers
- `200 {allowed: false, status_code: N, reason: R}` → HTTP `N` with reason body
- transport error / unparseable → HTTP `503` (fail closed, matching the Kong plugin)

Stateless, no storage, one upstream. Deployed as a normal Deployment + Service.

### 5.3 Guardrails

Set per-model on `AgentgatewayModel.policies.promptGuard`.

| Kong guard plugin | Agentgateway guard |
|---|---|
| `open-ai-guard-request` | `openAIModeration` — **native**, external service no longer needed |
| `keyword-guard-request` | `regex`, or `webhook` → the existing keyword service |
| `llama-ai-guard-request` | `webhook` → the existing Llama Guard service |
| `presidio-ai-guard-request` | `webhook` → Presidio analyzer/anonymizer |

**Deliberate simplification:** Kong attaches all four guards to every route with
`external_config: true` and placeholder URLs, then has the Rust server inject per-tenant guardrail
config at request time via `kong.ctx.shared.ai_gw_guardrails`. Agentgateway has no equivalent
config-injection path — guardrail config is static per-route/per-model policy.

The pilot therefore configures guardrails **statically per model**. Per-tenant guardrail variation
is out of scope for Phase 1 and is the single largest architectural gap in the migration
(see §9).

**Net gain:** the pilot gets `promptGuard.response` and `promptGuard.streaming: Enabled`, neither of
which exists in the Kong stack.

---

## 6. Token Accounting — Known Limitation

Kong's `ai-gateway` plugin reports actual token usage to the Rust server's `/v1/usage` in the log
phase (fire-and-forget via `ngx.timer.at`), correcting the pre-debited estimate.

Agentgateway has **no log-phase webhook**. Consequences for Phase 1:

- The Rust server's TPM pre-debit is never corrected. Since `estimated_tokens: 100` is far below
  typical real usage, tenant budgets are **under-debited**.
- This is contained by the pilot running its own Rust server instance and database (§1.1), so
  production accounting is unaffected.

Two options for Phase 2, to be decided after the pilot:

1. **`ext_proc`** — agentgateway's external processing supports `ResponseHeaders` and
   `ResponseBody` (including `FullDuplexStreamed`), so a small service can observe responses and
   POST to `/v1/usage`. Highest fidelity; adds a service to the response path.
2. **Telemetry-derived** — consume agentgateway's OTLP per-token-type metrics or its
   SQLite/Postgres usage persistence and reconcile in batch. Cheaper, but asynchronous and not
   per-request.

---

## 7. Observability

| Concern | Implementation |
|---|---|
| Metrics | `frontend.metrics`; the agentgateway Helm chart ships `monitoring.yaml` (PodMonitor/ServiceMonitor). Scrape into the existing `monitoring` namespace. |
| Tracing | `frontend.tracing` — W3C traceparent. New capability; Kong only has core tracing. |
| Access logs | `frontend.accessLog`, replacing the Kong `file-log` central-log plugin. |
| Usage analytics | Agentgateway usage persistence (SQLite/Postgres), replacing `serialize-analytics`. |

**Not available:** Kong's `ai_proxy_ttft_ms` / `ai_proxy_tpot_latency_ms` serving histograms have no
agentgateway equivalent. Comparative latency analysis against Kong must use client-side
measurement.

---

## 8. Error Handling & Failure Modes

| Failure | Behavior | Config |
|---|---|---|
| Rust server down / adapter error | Deny with 503 | `extAuth.failureMode: deny` + adapter fail-closed |
| Guardrail service unreachable | Fail closed by default; per-guard override | `promptGuard` failure mode |
| Provider 5xx / connection failure | Evict backend, retry next | `policies.health` eviction + `traffic.retry` |
| Provider 429 | Evict for `Retry-After` duration | `health.unhealthyExpression: "response.code == 429"` |
| All backends evicted | LB serves from the rejected pool rather than hard-failing | Built-in `select_fallback` |
| Request body > 64 KiB | **Retries silently disabled** | Hardcoded `MAX_BUFFERED_BYTES`; see §9 |

### 8.1 Retry configuration caution

Agentgateway's `attempts` field documents itself as "total number of attempts, including the
original request," but the implementation computes `attempts + 1`. Setting `attempts: 1` produces
**two** upstream calls. Configure with this in mind and verify against logs.

---

## 9. Known Gaps and Open Items

| # | Gap | Severity | Disposition |
|---|---|---|---|
| 1 | Per-tenant guardrail config injection | **High** | No agentgateway equivalent today. Phase 1 uses static per-model guardrails; **candidate for a code change in Phase 6** (§9.1). |
| 2 | `/v1/usage` TPM true-up | **High** | Deferred; contained by the isolated Rust instance. Phase 2 via `ext_proc`, or **candidate for a code change in Phase 6** (§9.1). |
| 3 | VNG IAM token exchange for `aiplatform_vllm` | Medium | Requires a shim; exercised by slice model #5. |
| 4 | Web search / server-tool loop | Medium | Deferred. No extension point exists. |
| 5 | `body.model` vs path-segment 400 check | Low | Accepted (§3.1). Restorable via CEL. |
| 6 | TTFT/TPOT serving histograms | Low | Not available; use client-side measurement. |
| 7 | Retry disabled above 64 KiB request body | Low for pilot | Hardcoded upstream. Affects large-prompt traffic; worth an upstream issue. |

### 9.1 Patching agentgateway (accepted for a later phase)

Modifying agentgateway itself is an accepted option for closing gaps 1 and 2, rather than working
around them. Both are contained changes in Rust, and both are plausibly upstreamable — neither is
VNG-specific. Sketches, to be designed properly when that phase starts:

**Gap 2 — usage reporting hook.** The smaller of the two. Agentgateway already extracts per-token-type
usage for telemetry and cost projection (`llm/cost/`), so the data exists at the right point in the
response path. The change is an outbound "usage callback" policy that POSTs it to a configured
endpoint after the response completes — the direct analogue of the Kong plugin's fire-and-forget
`ngx.timer.at` to `/v1/usage`. Avoids putting `ext_proc` on the response path.

**Gap 1 — guardrail config from ext_authz.** The larger change. Kong's pattern is that the policy
server returns per-tenant guardrail config, which the guard plugins read from request context. The
agentgateway analogue would let the `extAuth` response supply values into the request context that
`promptGuard` then consults, making guardrail config dynamic per request rather than static per
route. This touches the policy model, not just a filter, so it needs its own design pass.

**Sequencing note:** doing either before Phase 5 would confound the comparison — the pilot should
first be measured against Kong using stock agentgateway, so the gaps are quantified before they are
engineered away.

---

## 10. Acceptance Criteria

The pilot is judged successful when all of the following hold:

1. A request to a legacy Kong URL, with an unmodified body, returns a correct response through
   agentgateway for all 5 slice models.
2. Streaming (SSE) works end-to-end for chat completions.
3. An unauthorized request is rejected with the same status code Kong returns, proving the
   adapter's fail-closed path.
4. The weighted DeepSeek split distributes traffic approximately 99:1 across the two backends,
   verified by metrics.
5. A guardrail rejection occurs and is observable in metrics and access logs.
6. A forced provider failure (429 or 5xx) triggers eviction and a successful retry.
7. Metrics are scraped and visible alongside the Kong dashboards.
8. No credential appears in plaintext in any manifest.

---

## 11. Phasing

**Phase 1 — Foundation.** Install CRDs + controller; create the namespace, `Gateway`, and TLS;
verify the LoadBalancer and a trivial route end-to-end.

**Phase 2 — Models.** Add the 7 `AgentgatewayModel` CRs (5 public, 2 internal) and the 3 HTTPRoutes
with URL rewrites. Validate each shape against acceptance criteria 1–2, 4.

**Phase 3 — Governance.** Deploy the pilot Rust server + adapter; wire `traffic.extAuth`; add
static guardrails. Validate criteria 3, 5.

**Phase 4 — Resilience & observability.** Health/eviction policies, retry, metrics, tracing,
access logs. Validate criteria 6–8.

**Phase 5 — Comparison.** Replay captured Kong traffic against both gateways; compare correctness,
latency, and cost. Produce a go/no-go recommendation on the §9 gaps.

**Phase 6 — Code changes (conditional).** If Phase 5 says continue, close gaps 1 and 2 by patching
agentgateway per §9.1. Deliberately sequenced last so the gaps are measured before being engineered
away.
