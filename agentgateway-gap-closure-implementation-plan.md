# Agentgateway — Gap-Closure Implementation Plan & Effort Estimate

**Date:** 2026-06-24
**Goal:** Reimplement, *into agentgateway* (this Rust project), the capabilities that the custom Kong AI-Proxy has and agentgateway currently lacks (from §15 of the comparison report).
**Basis:** Estimates are grounded in the actual agentgateway codebase (`crates/agentgateway/src/`), not generic guesses. See the integration-point findings in the Appendix.

---

## 0. TL;DR — Headline Numbers

| Delivery tier | What you get | Engineer-effort | Calendar (1 eng) | Calendar (2 eng) |
|---|---|---|---|---|
| **Tier A — Per-instance MVP** | All features working **within a single gateway process** (no cross-pod coordination). Capacity/sticky/cooldown are accurate per-instance only. | **~45–65 eng-days** | ~9–13 wks | ~6–8 wks |
| **Tier B — Cross-instance parity** | True Kong-equivalent behavior across N gateway pods via a new shared-state (Redis) subsystem. | **~72–104 eng-days** | ~15–21 wks | ~9–13 wks |
| **+ Optional add-ons** | Native Cohere/DeepSeek/Mistral/HF/Llama2 drivers (+10–15d) and image-generation route types (+4–6d). | +14–21 eng-days | — | — |

> **The single biggest driver of cost is architectural:** agentgateway keeps **all** health/capacity/eviction state **in-process and in-memory** (`types/loadbalancer.rs`), with **no Redis or shared store anywhere** in the codebase. The Kong fork's headline features (sticky api-key→farm affinity, concurrent/TPM capacity counters, failover cooldowns) are **only meaningful across pods** when coordinated through Redis. Faithful parity therefore requires building a brand-new shared-state subsystem first (Tier B, Phase 0.2). If you can accept per-pod approximation, Tier A skips that and is ~35% cheaper.

Estimates assume **one mid/senior Rust engineer who has ramped on this codebase**. They include testing but not large-scale load testing of the GPU farm itself. Add ~20% buffer for review cycles, CI, and integration friction if you want a commit-level plan.

---

## 1. Scope — The Gap Features to Port

From the comparison report (§15, "What the Kong fork has that Agentgateway lacks"), grouped into workstreams:

| # | Gap feature (Kong has, AGW lacks) | Workstream | Priority |
|---|---|---|---|
| 1 | Self-contained capacity gating — concurrent / TPM-window / Prometheus strategies | **WS-1 Capacity** | High |
| 2 | Cross-instance sticky session→backend affinity (Redis-backed) | **WS-2 Sticky** | High |
| 3 | Explicit failover cooldown state machine + **active** health probing | **WS-3 Failover/Health** | High |
| 4 | Least-loaded overflow soft-degrade | WS-3 | Med |
| 5 | VNG Cloud MaaS drivers (`aiplatform`, `aiplatform_vllm`) + VNG IAM auth | **WS-4 Providers** | High |
| 6 | Serving-SRE telemetry (sticky hit-rate, capacity rejections, failover reasons, balance picks) | **WS-5 Telemetry** | Med |
| 7 | Web search tool injection | **WS-6 WebSearch** | Low |
| 8 | Image generation route types | WS-4 | Low |
| 9 | Capacity-plan / AC-vs-AG workload segregation / runbooks | **WS-7 Ops/Docs** | Med |
| 10 | Native Cohere/DeepSeek/Mistral/HF/Llama2 drivers | WS-4 (optional) | Low |

**Already present in agentgateway — do NOT reimplement** (clarifying scope so we don't over-estimate):
- **TTFT / TPOT / e2e latency** histograms already exist (`telemetry/metrics.rs`: `gen_ai_time_to_first_token`, `gen_ai_time_per_output_token`; first-token instant captured in `llm/conversion/*`). WS-5 only adds the *routing/capacity* metrics.
- **Cost accounting** is already richer in agentgateway (price catalog with tiers) than Kong — no port needed.
- **Cohere/DeepSeek/Mistral/HF/Llama2** are already reachable via the **Custom OpenAI-compatible provider** (`llm/custom.rs`). Native drivers (#10) are an *optional polish*, not a functional gap.
- Weighted + failover routing already exists (`llm/model_router.rs`); WS-3 adds the *cooldown/health/least-loaded* refinements on top.

---

## 2. Architecture Strategy

agentgateway's extension model is clean and trait-based, which keeps most features cheap **except** where cross-pod state is required:

- **Policies** plug in via `RequestPolicyTrait` / `ResponsePolicyTrait` / `BackendPolicyTrait` (`store/policy.rs`), are added as a `TrafficPolicy` enum variant, wired into `apply_request_policies()` (`proxy/httpproxy.rs`), and configured via serde structs with `#[apply(schema!)]` auto JSON-schema. → capacity gating, sticky, web-search all fit this pattern.
- **Endpoint selection** lives in `types/loadbalancer.rs` (`EndpointWithInfo`, `EndpointInfo`, `select_p2c`, `score()`). Capacity eligibility filtering and least-loaded overflow hook in here.
- **Providers** implement a trivial `Provider` trait (`const NAME`) + conversion modules (`llm/conversion/*`). New drivers are mostly format-conversion + auth code.
- **Metrics** are `Family<Labels, Counter/Histogram>` fields on the `Metrics` struct (`telemetry/metrics.rs`) — ~20 lines each.
- **Background tasks** (active health prober, Prometheus poller) have **no existing pattern** — must be spawned at startup and lifecycle-managed.
- **Shared state** has **no existing pattern** — the only cross-instance mechanism today is the *remote* rate-limit gRPC client (`http/remoteratelimit.rs`). Tier B builds a Redis-backed KV abstraction modeled loosely on that.

**Recommendation:** Build a single reusable `SharedStore` abstraction (Phase 0.2) with two backends — `InMemory` (Tier A / single-pod / dev) and `Redis` (Tier B) — behind one trait (`get/incr/decr/setex/setnx_ex` with TTL). Every stateful feature (capacity counters, sticky bindings, cooldowns, poller cache) is then backend-agnostic. This turns Tier A→Tier B into a config switch rather than a rewrite, and is the highest-leverage decision in the plan.

---

## 3. Phased Plan with Estimates

Estimates in **engineer-days (d)**. Confidence: 🟢 high · 🟡 medium · 🔴 low (research-heavy).

### Phase 0 — Foundations *(serial prerequisite)*

| Task | Detail | Est. | Conf. | Tier |
|---|---|---|---|---|
| 0.1 Design spike & ADR | Read selection/policy/telemetry paths; design `SharedStore` trait, capacity policy interface, background-task lifecycle; write ADR | 3–5d | 🟢 | A+B |
| 0.2 SharedStore subsystem | Trait + `InMemory` backend (atomic maps w/ TTL) + `Redis` backend (pooled `redis-rs`/`fred`, `incr/decr/expire/setnx`, fail-open vs fail-closed modes), config wiring, schema, unit+integration tests | **8–12d** | 🟡 | **B only** |

*Tier A skips 0.2's Redis backend (uses in-memory only): Phase 0 = 3–5d. Tier B Phase 0 = 11–17d.*

### Phase 1 — WS-1 Capacity Gating

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 1.1 Capacity policy framework | New `capacitylimit.rs`; `RequestPolicyTrait` (reserve pre-dispatch, reject when over) + `ResponsePolicyTrait` (release); `TrafficPolicy` variant; config struct + schema; store reservation in request extensions | 4–5d | 🟢 |
| 1.2 Concurrent strategy | In-flight counter via SharedStore, `route`/`global` scope, lease TTL safety net | 2–3d | 🟢 |
| 1.3 TPM strategy | Trailing 60s bucket windows w/ linear decay, in/out/all axes; char-÷-ratio prompt estimate at admission; **true-up** from response/stream usage at completion | 5–7d | 🟡 |
| 1.4 Prometheus capacity poller | Background task: PromQL scalar fetch on interval, dedup keys, generation kill-switch, SHM/store cache, fail-open on stale/error, error metric | 4–5d | 🟡 |
| 1.5 Selection eligibility | Filter over-capacity endpoints before `select_p2c` in `loadbalancer.rs`; emit `capacity_rejected` | 3–4d | 🟡 |
| **Phase 1 subtotal** | | **18–24d** | |

### Phase 2 — WS-2 Sticky Affinity

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 2.1 Sticky key extraction | Keys from `api_key_id` / header / cookie / `+ip`; SHA-hash/mask client id | 2d | 🟢 |
| 2.2 Binding store + selection | Bind→backend in SharedStore w/ TTL refresh; fallback chain sticky→weighted→least-loaded; integrate into selection. (Note: existing `sessionpersistence.rs` is *stateless cookie* encoding the addr — different model, partial reuse only) | 4–6d | 🟡 |
| 2.3 Metrics | `sticky_total` hit/miss/collision | 1d | 🟢 |
| **Phase 2 subtotal** | | **7–9d** | |

### Phase 3 — WS-3 Failover Cooldown, Active Health, Least-Loaded

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 3.1 Cooldown state machine | On response status in `failover_on` set, mark target failed w/ cooldown TTL in SharedStore; exclude from selection; `target_marked_failed`/`failover_total` metrics. (Builds on existing passive `http/health.rs` eviction) | 3–4d | 🟡 |
| 3.2 Active health prober | Background per-endpoint timer; tiny chat-ping payload (Anthropic-aware: no `max_completion_tokens`); status TTL; generation kill-switch; `health_probe_total` | 5–7d | 🟡 |
| 3.3 Least-loaded overflow | When all eligible at capacity, soft-degrade to lowest in-flight count; `capacity_soft_degrade` metric | 2–3d | 🟢 |
| 3.4 Fallback-overlay merging | Blend partial fallback config (model/url) onto primary request config | 2–3d | 🟢 |
| **Phase 3 subtotal** | | **12–17d** | |

### Phase 4 — WS-4 Providers & Route Types

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 4.1 VNG Cloud `aiplatform` driver | Native driver or Custom-provider config; request/response conversion | 3–5d | 🟡 |
| 4.2 VNG Cloud `aiplatform_vllm` driver | OpenAI-compatible (SGLang) — mostly config + endpoint/auth wiring | 2–3d | 🟢 |
| 4.3 VNG Cloud IAM backend auth | New auth provider (`http/auth/`): exchange access/secret key → bearer token at `iam_auth_url`, cache w/ TTL < expiry | 4–5d | 🟡 |
| **WS-4 core subtotal (VNG)** | | **9–13d** | |
| 4.4 *Optional* native drivers | Cohere/DeepSeek/Mistral/HF/Llama2 (~2–3d each; OpenAI-compat ones cheaper). *Functionally already covered by Custom provider* | +10–15d | 🟡 |
| 4.5 *Optional* image route types | `images/generations|edits|variations` + conversion | +4–6d | 🟡 |

### Phase 5 — WS-5 Serving-SRE Telemetry

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 5.1 Routing/capacity metrics | `balance_picks_total` (by strategy), `balance_exhausted_total`, `capacity_in_flight`/`capacity_max` gauges, `tpm_reserved`/`tpm_trueup_delta`, `prom_poll_error_total`. (Most emitted from WS-1/2/3; this is wiring + dashboards.) TTFT/TPOT/e2e **already exist** | 3–5d | 🟢 |
| **Phase 5 subtotal** | | **3–5d** | |

### Phase 6 — WS-6 Web Search Injection

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 6.1 Web-search policy | Request policy injecting web_search tool / transforming Anthropic tools to upstream formats | 3–5d | 🔴 |
| **Phase 6 subtotal** | | **3–5d** | |

### Phase 7 — WS-7 Ops, Workload Segregation & Docs

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 7.1 AC vs AG segregation | Mostly achievable with existing per-route backends + timeouts; deliver example configs + per-route timeout/queue guidance | 2–3d | 🟢 |
| 7.2 Capacity-plan & runbook docs | Port architecture/capacity/incident docs to agentgateway terms | 2–3d | 🟢 |
| **Phase 7 subtotal** | | **4–6d** | |

### Phase 8 — Integration, Hardening, E2E

| Task | Detail | Est. | Conf. |
|---|---|---|---|
| 8.1 End-to-end + multi-instance tests, failure-mode/chaos, perf sanity, docs polish | | 5–8d | 🟡 |

---

## 4. Totals

| Bucket | Tier A (per-instance) | Tier B (cross-instance parity) |
|---|---|---|
| Phase 0 Foundations | 3–5d | 11–17d |
| Phase 1 Capacity | 14–19d* | 18–24d |
| Phase 2 Sticky | 5–7d* | 7–9d |
| Phase 3 Failover/Health | 11–15d* | 12–17d |
| Phase 4 Providers (VNG core) | 9–13d | 9–13d |
| Phase 5 Telemetry | 3–5d | 3–5d |
| Phase 6 Web search | 3–5d | 3–5d |
| Phase 7 Ops/Docs | 4–6d | 4–6d |
| Phase 8 Integration | 4–6d | 5–8d |
| **TOTAL (core)** | **~45–65 eng-days** | **~72–104 eng-days** |
| Optional native drivers | +10–15d | +10–15d |
| Optional image route types | +4–6d | +4–6d |

\* Tier A figures are lower because the in-memory store removes Redis-coordination work in capacity/sticky/cooldown.

**Calendar translation** (core scope, ~20% PM/review buffer folded in to the ranges already):
- **Tier A:** 1 engineer ≈ **9–13 weeks**; 2 engineers ≈ **6–8 weeks**.
- **Tier B:** 1 engineer ≈ **15–21 weeks (≈3.5–5 months)**; 2 engineers ≈ **9–13 weeks (≈2–3 months)**.

Two engineers don't halve Tier B because **Phase 0.2 (SharedStore) is a serial bottleneck** that most other phases depend on.

---

## 5. Sequencing & Dependencies

```
Phase 0.1 (spike)──┬─> 0.2 SharedStore (Tier B) ──┬─> WS-1 Capacity ──┐
                   │                               ├─> WS-2 Sticky    ├─> WS-5 Telemetry ─> Phase 8
                   │                               └─> WS-3 Failover  ┘        (cross-cutting)
                   └─> WS-4 Providers (independent — can start immediately, no shared state)
                   └─> WS-6 Web search (independent)
                   └─> WS-7 Ops/Docs (after WS-1/3 land)
```

**Parallelization plan for a 2-engineer team:**
- **Eng A (state track):** 0.1 → 0.2 → WS-1 → WS-3 → WS-2 → WS-5 → Phase 8.
- **Eng B (provider/independent track):** WS-4 (VNG drivers + IAM) → WS-6 web search → optional native drivers → WS-7 docs → join Phase 8.

This keeps Eng B fully utilized on the no-shared-state features while Eng A builds the foundation.

---

## 6. Risk Register

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| SharedStore subsystem under-scoped (TTL semantics, Redis HA failover, fail-open/closed correctness) | High | Med | Time-box 0.1 spike; model on `remoteratelimit.rs`; pick mature crate (`fred`); explicit fail-mode tests |
| TPM true-up accuracy (estimate vs actual divergence) | Med | Med | Reconcile from real usage metadata at completion (already extracted for cost); alert on large deltas |
| Active prober adds load / false signals to GPU farms | Med | Med | Long intervals (30s), tiny payloads, generation kill-switch, per-endpoint dedup |
| Web search injection semantics ill-specified | Med | Med | Lowest priority; spec against Kong `web_search.lua` behavior first (🔴 confidence) |
| Maintaining a fork vs upstream agentgateway | Med | High | Implement as **upstream-style policies/providers** (clean trait impls) to ease rebasing or upstreaming |
| Cross-instance correctness under churn (rebalancing, sticky thrash) | Med | Med | Multi-instance E2E + chaos tests in Phase 8 |

---

## 7. Recommended Approach (fastest value first)

1. **Decide Tier A vs B early.** If you run **one gateway replica per farm region** (or can tolerate per-pod approximation), **Tier A** is dramatically cheaper and may be sufficient — many capacity/sticky benefits hold within a pod. Only commit to Tier B if you truly run **N load-balanced gateway pods that must agree**.
2. **Ship WS-4 (VNG drivers + IAM) first** — it's independent of the state subsystem, unblocks real traffic to your MaaS, and gives immediate value while the foundation is built.
3. **Then WS-1 Capacity + WS-3 Failover/Health** — the core operational value for GPU-farm protection.
4. **WS-2 Sticky + WS-5 Telemetry** — optimization + visibility.
5. **WS-6 web search, optional drivers, image types** — defer; lowest functional value (Custom provider already covers most providers).

> **Pragmatic minimum** to get agentgateway "good enough" for your MaaS serving topology: **WS-4 + WS-1 + WS-3 (Tier A)** ≈ **30–45 engineer-days (~6–9 weeks solo)**. This delivers the VNG integration, capacity gating, and failover/health — the features that actually matter for GPU-farm serving — without the full cross-instance Redis investment.

---

## Appendix — Key Integration Points (verified in code)

- **Endpoint model / selection / scoring:** `types/loadbalancer.rs` — `EndpointWithInfo`, `EndpointInfo` (in-memory `health` EWMA, `pending_requests`, `consecutive_failures`, `evicted_until`), `select_p2c` (power-of-two), `score()`. **State is per-process, atomic, NOT shared.**
- **No distributed state anywhere.** Local rate limit = in-memory token bucket (`http/localratelimit.rs`); only cross-instance mechanism is the remote rate-limit gRPC client (`http/remoteratelimit.rs`). Session persistence (`http/sessionpersistence.rs`) is stateless encrypted-cookie, not a server-side store.
- **Policy hooks:** `store/policy.rs` traits (`RequestPolicyTrait`/`ResponsePolicyTrait`/`BackendPolicyTrait`); pipeline order in `proxy/httpproxy.rs::apply_request_policies()`; register via `TrafficPolicy` enum (`types/agent.rs`).
- **Config/schema:** serde structs + `#[apply(schema!)]` auto JSON-schema; YAML→runtime mapping in `types/local.rs`, `config.rs`. ~30–50 lines boilerplate per policy.
- **Providers:** `Provider` trait (`const NAME`) in `llm/mod.rs`; `AIProvider` enum; conversion in `llm/conversion/*`; `llm/custom.rs` already supports OpenAI-compatible endpoints. Typical driver 100–500 lines.
- **Metrics:** `telemetry/metrics.rs` `Metrics` struct of `Family<Labels, …>`; ~20 lines per metric. **TTFT/TPOT/e2e already implemented.**
- **Health:** passive only (`http/health.rs` reactive eviction, `http/outlierdetection.rs` Retry-After parsing). **No active prober and no background-task pattern exists yet** — must be built.
