# G6 Sticky Affinity + G7 Capacity Caps — Design

**Status:** Forward-looking design (spec + seam). No code ships now.
**Scope:** agentgateway → Kong-ai-proxy parity, maas-v2 model-routed surface.
`feat/maas-v2-parity`, repo `~/agentgateway`.
**Date:** 2026-09-08.

## Purpose

Close the two gaps the parity plan (`curried-moseying-pelican.md`) deferred as
"absent on all 660+ live routes" with explicit triggers:

- **G6 FR-3.3 sticky affinity** — trigger: a route needs api-key→backend pinning
  (e.g. prompt-cache affinity).
- **G7 FR-3.5 capacity caps** — trigger: a route needs TPM-gating or inflight
  capacity protection (e.g. GPU saturation).

This document is the design so both are ready to build when a trigger lands.
It does not ship code. G1-G5 (the live parity gaps) are already committed on
`feat/maas-v2-parity` (`9ade0299` and prior).

## Decisions (locked during brainstorm)

| Decision | Choice | Rationale |
|---|---|---|
| Trigger timing | Forward-looking only | No live route needs G6/G7; complete the design so they're ready |
| State home | **Per-pod only** | Parity plan premise: one replica, per-pod suffices. Cheapest; no new SharedStore; no Tier-2 integration |
| G6 key | **api-key sha256** | Already resolved at route auth (`httpproxy.rs:191`); session-id doesn't exist as a concept |
| G6 pin hardness | **Soft preference** | Prompt-cache affinity wants "prefer same backend, don't die if gone"; no head-of-line blocking |
| G7 signal | **Inflight cap + per-endpoint TPM** | Most protective; inflight counter already exists, TPM pre-debit/true-up feasible (input_tokens available pre-dispatch) |
| Approach | **A: `SelectionContext`** | One signature ripple serves both; G6 soft-preference + G7 hard-gate compose in one predicate |

## Verified architecture facts (four explorations)

### agentgateway runtime

- **`sessionpersistence.rs` is an inert stub.** `Policy{}` empty struct
  (`sessionpersistence.rs:4`); `apply_backend_policies` literally discards it:
  `// TODO: implement session persistence` + `session_persistence: _`
  (`httpproxy.rs:330-331`). The AES-cookie encoder + `HTTPSessionState{backend}`
  shape exist but are unwired and **not reused by this design** — G6 uses a
  plain `DashMap<key, PinEntry>` (no cookie, no stateless-cookie round-trip;
  state is per-pod in-memory, not client-carried). The only wired affinity today
  is MCP `override_dest` (`binds.rs:273`, "Not exposed through config").
- **No selection function takes an identity param.** `select_provider`
  (`llm/mod.rs:70`), `select_endpoint` (`loadbalancer.rs:229`), `best_bucket`
  (`:614`), `viable` (`:355`), `find_endpoint` (`:673`) — all `&self` only. The
  api-key is validated at route auth (`httpproxy.rs:191`) but never threaded
  into selection. Threading it is the real G6 cost.
- **Inflight is already tracked per endpoint, soft-only.** `EndpointInfo.
  pending_requests: ActiveCounter` (`loadbalancer.rs:881`, via `Arc::strong_count()`),
  incremented in `start_request` (`:936`), decremented on `ActiveHandle::drop`.
  `score()` (`:925`) uses it only as a latency multiplier, never a hard gate.
- **`localratelimit.rs` has a token-bucket with a `Tokens` mode** (`:52-55`),
  route-scoped (not per-endpoint), applied at `httpproxy.rs:205-208`.
  `check_llm_request` (`:88`) pre-debits `req.input_tokens`. This is the
  template for per-endpoint TPM.
- **No `capacitylimit.rs`, no TPM/inflight gate.** Confirmed absent.
- **`LLMRequest.input_tokens: Option<u64>`** is available pre-dispatch
  (`httpproxy.rs:499` passes it to `check_llm`). TPM pre-debit is feasible.
- **`BackendPolicies`** (`binds.rs:242-274`) has `session_persistence: Option<
  http::sessionpersistence::Policy>` (empty) and `health`, but **no sticky /
  capacity / affinity / tpm / inflight field**.
- **ext-authz is wired** (`httpproxy.rs:196,352,430`) but Envoy-style
  (`envoy.service.auth.v3.CheckRequest`), **not** the Tier-2 plugin-server's
  `/v1/check`+`/v1/usage` contract. agentgateway does **not** call the
  ai-gateway-plugin-server today.

### agentgateway controller

- **No Sticky/Session/Affinity/Capacity/Inflight/TPM CRD field** anywhere
  (`agentgateway_{policy,model,backend,parameters,shared,overlay}_types.go`,
  `ai_policy.go`). The only load hint is `FailoverModelTarget.Priority`
  (`agentgateway_model_types.go:353`).
- **`BackendPolicySpec` oneof** (`resource.proto:1661-1679`) has 18 cases;
  none sticky/capacity. New leaves add cases 19 (`Sticky`) and 20 (`Capacity`).
- **Health is the wiring template** (CRD → proto → translator → Rust bind):
  `translateModelPolicies` (`model_collections.go:430`) →
  `TranslateInlineBackendPolicy` (`backend_policies.go:178`) reading
  `backend.Health` → `translateBackendHealthPolicy` (`:300`) →
  `api.BackendPolicySpec_Health` → Rust `BackendTrafficPolicy::Health`
  (`binds.rs:~1362`). Sticky/Capacity follow this exact 5-step path.
- **`sessionpersistence::Policy` is runtime-only** — no CRD producer, no proto
  oneof case. Wiring G6 requires a new CRD field + proto case before the Rust
  bind can receive it.

### Tier-2 policy plane (ai-gateway-plugin-server)

- Redis/Dragonfly pool (`fred`, `RateLimitState` at `infra/state.rs:38`) + one
  reusable atomic Lua executor (check-then-increment + rollback, integer-only).
- api-key sha256 **is already the bucket key** for `per_key`/`per_key_model`
  counters (`rate_limit.rs:571-581`).
- Pipeline-stage extension point (`Stage` enum, fail-closed on unknown;
  `gateway_mode` added 2026-08-24 as precedent).
- `/v1/usage` post-response hook exists (TPM true-up) — natural decrement point.
- **No affinity map** (integer-only Lua); **no inflight counter** (RPM expires
  with window; TPM is delta-trued, not released).
- **agentgateway does NOT speak this contract** — reusing Tier-2 is a new
  cross-service integration, not an extension. Out of scope for per-pod parity.

### SharedStore (June plan Phase 0.2)

- **Zero trace** in controller, proto, or runtime. Genuinely greenfield.
- Redundant if per-pod suffices (the parity premise) or if Tier-2 externalizes
  (out of scope). Not built.

## Architecture

### The shared seam: `SelectionContext` (Approach A)

Both G6 and G7 hook endpoint selection, which today happens in a chain where
no function receives caller identity or request size:

```
httpproxy.rs:2041  BackendCall::build
  └─ AIBackend::select_provider()        llm/mod.rs:70   — &self only
       └─ P2C over [a,b]                 llm/mod.rs:82   — THE SEAM
            └─ score()                   loadbalancer.rs:925 — health/latency/inflight-soft
  └─ EndpointSet::start_request()        loadbalancer.rs:585 — mints ActiveHandle, incs pending_requests
```

**Change:** one struct threaded through the AI-path chain:

```rust
pub struct SelectionContext<'a> {
    pub key: Option<&'a str>,        // api-key sha256, for G6 sticky; None when no policy
    pub input_tokens: Option<u64>,   // LLMRequest.input_tokens, for G7 TPM pre-debit
}
```

`select_provider` gains `&SelectionContext`; the P2C `map` at `llm/mod.rs:82`
becomes the **single place** where (a) sticky bias and (b) capacity-gate
predicates evaluate, in one composed ordering. `best_bucket` and `viable` get
the same context for symmetry (the AI path is the parity-relevant one).

**Why one struct, not two params:** G6's soft-preference and G7's hard-gate
must compose — a pinned endpoint over its inflight cap should lose to an
unpinned under-cap endpoint, but still beat an unavailable one. That ordering
only exists if both predicates see the same candidate in the same closure.

**Ripple scope:** `select_provider`, `best_bucket`, `viable`, `find_endpoint`,
and the `BackendCall` site that builds the context (api-key sha256 from
`:191`, `input_tokens` from `LLMRequest`). Leaves take `Option<&SelectionContext>`
— `None` when no policy → today's behavior, zero overhead.

### Per-pod state

`SelectionState` hung off `BackendWithPolicies` (next to the
`ProberGenerationRegistry` from G4):

```rust
struct SelectionState {
    pins: DashMap<Strng, PinEntry>,          // G6: key→backend, TTL'd
    tpm: DashMap<Strng, TpmCounter>,          // G7: endpoint→TPM window
}
```

- `pins`: `PinEntry { backend_name: Strng, expires_at: Instant }`, keyed by
  api-key sha256, TTL'd.
- `tpm`: `TpmCounter { window_start, debited: AtomicU64, actual: AtomicU64 }`
  per endpoint.
- `EndpointInfo.pending_requests` is reused unchanged for G7 inflight — no move.

### CRD → proto → runtime (one path, two leaves)

Both `Sticky` and `Capacity` are backend-policy leaves under `BackendFull`
(sibling to `Health`/`Retry`), following the mapped Health 5-step path:

| Step | Sticky | Capacity |
|---|---|---|
| 1. CRD struct | `Sticky{Key, TTL}` | `Capacity{InflightCap, TpmPerMinute, Cooldown}` |
| 2. proto oneof | `BackendPolicySpec_Sticky` (case 19) | `BackendPolicySpec_Capacity` (case 20) |
| 3. translator | `translateBackendStickyPolicy` + branch in `TranslateInlineBackendPolicy:178` | `translateBackendCapacityPolicy` + same branch |
| 4. regen | `api/resource.pb.go` (shared) | (shared) |
| 5. Rust bind | `BackendTrafficPolicy::Sticky(p)` (`binds.rs:~1355`) + `BackendPolicies.sticky` | `BackendTrafficPolicy::Capacity(p)` + `BackendPolicies.capacity` |
| Local path | `LocalSticky` + `TryFrom` (G4 pattern) | `LocalCapacity` + TryFrom |

## G6 — per-pod sticky affinity (soft preference)

### Policy config

```go
// controller/.../agentgateway_policy_types.go — under BackendFull, sibling to Health
type Sticky struct {
    // CEL expression resolving to the affinity key (default: the request's api-key sha256).
    // +optional
    Key *CELExpression `json:"key,omitempty"`
    // How long a pin survives after last use. Default 10m.
    // +optional
    TTL *metav1.Duration `json:"ttl,omitempty"`
}
```

### Runtime (soft-preference predicate)

At the P2C seam (`llm/mod.rs:82`), with `ctx` available:

1. **Resolve the pin:** `ctx.key` → `pinmap.get(key)`. If present, not expired,
   and the endpoint's `evicted_until == None` → *pinned-available*.
2. **Score composition** (per candidate, applied in the P2C `map`):

   | candidate state | score multiplier |
   |---|---|
   | pinned & under-cap (G7 inflight < cap + TPM clear) | ×2.0 (strong bias) |
   | pinned & over-cap | ×1.2 (soft bias; loses to any under-cap) |
   | unpinned & under-cap | ×1.0 |
   | unpinned & over-cap | ×0.5 |

   P2C's `max_by(score)` lands on pinned-under-cap when available, then
   unpinned-under-cap.

   *(If G7 is absent, the table collapses to pinned=×2.0 / unpinned=×1.0 — so
   G6 is independently shippable, just reasoned in G7's terms.)*

3. **Pin writes:** on **successful** `finish_request` only,
   `pinmap.insert(key, backend_name, now+ttl)`. No write on failure or eviction
   (failure shouldn't pin to a possibly-sick backend).

### G6 key behaviors (spec'd explicitly)

- **Inert without policy:** `ctx.key == None` → no pinmap lookup, no pin write.
  Today's selection verbatim.
- **Evicted pin falls through:** if the pinned endpoint is formally evicted
  (`evicted_until` set), treated as unpinned. G2's `best_bucket` / G4's prober
  already moved it to `rejected`, so `find_endpoint` won't return it active.
  The pin record stays (TTL'd) but is skipped; a later sweep that sees it
  healthy again picks it up. **No stuck-on-bad-backend.**
- **First request:** no pin → normal P2C selects one → success pins it.
- **Per-pod only:** pod-1 may pin key-A→backend-X, pod-2 →backend-Y. Prompt-cache
  hit rate degrades by replica count. **Stated limitation; acceptable on one replica.**
- **TTL refresh:** each successful request touching the pin bumps `expires_at`.
  Idle keys expire and rebalance — prevents permanent skew.

## G7 — per-pod capacity gate (inflight + TPM)

### Policy config

```go
type Capacity struct {
    // Max concurrent in-flight requests per endpoint. Reject 503+Retry-After at/above.
    // +optional
    InflightCap *int32 `json:"inflightCap,omitempty"`
    // Per-endpoint tokens-per-minute budget. Pre-debited by input_tokens at select,
    // trued-up on completion.
    // +optional
    TpmPerMinute *int64 `json:"tpmPerMinute,omitempty"`
    // Once an endpoint trips capacity, hold it rejected for this long before retrying
    // (mirrors health eviction_duration). Default 3s — reuses G5 Retry-After.
    // +optional
    Cooldown *metav1.Duration `json:"cooldown,omitempty"`
}
```

### Runtime — inflight hard cap (counter exists)

`EndpointInfo.pending_requests` already tracks inflight. At the P2C seam, a
candidate is *under-cap* iff `pending_requests.count() < inflight_cap`. If
**all** candidates in the chosen bucket are at/over cap → selection returns
`NoHealthyEndpoints` → G5's `Retry-After` path fires (503 +
`Retry-After: cooldown`). No new counter; no new decrement path (existing
`ActiveHandle` drop handles it). ~5 lines in the predicate.

### Runtime — per-endpoint TPM (new per-pod state)

```rust
TpmCounter {
    window_start: Instant,      // fixed 60s window
    debited: AtomicU64,         // pre-debited input_tokens at select
    actual: AtomicU64,          // trued-up on finish_request with real usage
}
```

- **At select:** `window = now.floor(60s)`. If `window != window_start` → reset
  (CAS both counters). If `debited + input_tokens > tpm_per_minute` → candidate
  is *over-cap* (same predicate slot as inflight). On selection,
  `debited += input_tokens`.
- **At `finish_request`:** `actual += total_tokens` (from the usage the logging
  path captures). Reconcile: `debited = debited - input_tokens + actual` (pre-debit
  replaced by truth). On window rollover, `actual` carries no debt forward —
  fresh window, fresh budget.

Mirrors `localratelimit.rs`'s `Tokens` mode (`check_llm_request` at `:88`,
same pre-debit/true-up shape) but per-endpoint, not per-route. `input_tokens`
from `LLMRequest.input_tokens` (`httpproxy.rs:499`).

### Composed predicate (G6 + G7 in one table)

| candidate | score multiplier | rejected? |
|---|---|---|
| pinned & under-cap (inflight+TPM both clear) | ×2.0 | no |
| pinned & over-cap | ×1.2 | no (soft; G6 pins, G7 doesn't hard-reject if alternatives exist) |
| unpinned & under-cap | ×1.0 | no |
| unpinned & over-cap | ×0.5 | no |
| (all candidates at/over cap) | — | **yes → 503 + Retry-After** |

### G7 key behaviors (spec'd explicitly)

- **Inert without policy:** no `Capacity` → no inflight check, no TPM state,
  `under-cap` always true → table collapses to G6-only.
- **TPM true-up correctness:** the pre-debit is conservative (over-counts if the
  real stream is shorter). True-up replaces debited estimate with actual, so a
  1000-token pre-debit on a 200-token real response frees 800 back to the window
  within the same window. **No phantom debt across windows** — reset clears both
  `debited` and `actual`. **Cross-window debt is intentionally not tracked:**
  it would require per-key statefulness the per-pod model disclaims. A key that
  overspent last minute gets a fresh window this minute. **Stated limitation.**
- **No double-count (parity scope):** agentgateway doesn't call the Tier-2 plane
  today, so this per-pod TPM is the sole gate — nothing to double-count.
  **If agentgateway later runs alongside Tier-2 TPM** (out of parity scope), both
  would gate independently and possibly reject disjointly; the spec flags this as
  a known limitation with a trigger (Tier-2 becomes authoritative; agentgateway's
  gate should be removed or widened).
- **Inflight per-pod under-count:** 2 pods each capping at 10 while the upstream
  truly accepts 15 → agentgateway permits 20. Acceptable on one replica; on
  multi-replica it's a soft cap. **Stated.**
- **Reject path reuses G5:** no new 503/Retry-After code —
  `NoHealthyEndpoints → SERVICE_UNAVAILABLE` (G5, `proxy/mod.rs:287`) already
  injects `Retry-After`. G7 just needs `cooldown` to flow as the eviction duration.

## Sizing (forward-looking: spec + seam, not full ship)

| Piece | Est. | Notes |
|---|---|---|
| `SelectionContext` ripple + composed predicate | 2-3d | ~6 fns; `None`-path preserves today |
| G6 runtime (pinmap, score bias, pin-write) | 2d | simple DashMap |
| G6 controller (CRD+proto+translator+bind) | 2-3d | Health-path template |
| G7 inflight (predicate + reject→G5) | 0.5d | counter exists |
| G7 TPM (TpmCounter, pre-debit, true-up) | 2-3d | localratelimit.rs template |
| G7 controller (CRD+proto+translator+bind) | 2-3d | shared regen with G6 |
| Tests | 3-4d | composed-predicate cases; pin-on-success; TPM true-up; reject→Retry-After |
| **Total (both, forward-looking build)** | **~14-18d** | shippable independently: G6 ~6-8d, G7 ~6-8d once the shared ripple (~2-3d) lands |

**Sequencing:** shared ripple first → G6 + G7 in parallel after (the two halves
only meet in the predicate table).

## Testing (what proves correctness)

- **G6:** pin-on-success (not on failure); evicted-pin falls through; TTL expiry
  rebalances; multi-key spread; `None`-ctx is a no-op.
- **G7 inflight:** 503+Retry-After when all over cap; under-cap passes; counter
  decrements on drop.
- **G7 TPM:** pre-debit rejects over-budget; true-up frees phantom debt within
  window; window reset clears both counters; cross-window no-debt stated+tested.
- **Composed:** pinned-over-cap loses to unpinned-under-cap (the ordering that
  justifies Approach A); pinned-under-cap wins; all-over-cap → 503.
- **Per-pod limitation:** multi-"pod" test (two `SelectionState`s) shows a key
  pins differently — documents the stated limitation in code.

## Risk register

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| `SelectionContext` ripple hits hot path | Med | Med | `Option<&ctx>` + `None`-fast-path; benchmark P2C delta |
| Per-pod TPM double-counts with Tier-2 | Med | Low (parity scope) | Stated limitation + trigger to remove/widen when Tier-2 gates TPM |
| Soft-pin head-of-line wait | Low | Low | Soft (not hard) — pinned-over-cap loses to unpinned-under-cap by design |
| `DashMap` memory unbounded | Low | Low | TTL expiry on pins; TPM windows reset on rollover; bounded by active keys |
| CRD proto oneof addition is a schema break | Low | Low | Additive (new cases); old runtimes ignore unknown; verify with `kubectl apply --dry-run=server` |
| TPM pre-debit estimate wildly off | Med | Med | True-up within window frees phantom debt; cross-window no-debt is the accepted tradeoff |

## Stated limitations (per-pod model — accept or trigger Tier-2)

1. **G6 cross-pod pin divergence:** a key may pin to different backends on
   different pods. Prompt-cache hit rate degrades by replica count. Acceptable
   on one replica.
2. **G7 inflight per-pod under-count:** multi-pod caps sum, not min, against the
   upstream's true limit. Soft cap on multi-replica.
3. **G7 TPM no cross-window debt:** an overspending key gets a fresh window each
   minute. No per-key statefulness.
4. **G7 TPM double-count risk with Tier-2:** only if agentgateway later runs
   alongside the Tier-2 plane's TPM gate (out of parity scope). Trigger to
   remove/widen agentgateway's gate.

**Trigger to revisit state home:** if any limitation becomes load-bearing on a
live route, the state home decision flips to Tier-2 policy plane (shared Redis,
api-key bucket identity already there) or a new in-agentgateway SharedStore —
both out of this design's scope.

## Out of scope

- Building G6/G7 now (forward-looking only).
- SharedStore subsystem (June plan Phase 0.2) — zero trace, redundant per-pod.
- Tier-2 (ai-gateway-plugin-server) integration — agentgateway doesn't speak
  its contract; new cross-service integration, not an extension.
- Session-id affinity (no session concept exists; use api-key).
- Hard-pin G6 (soft chosen; hard risks head-of-line blocking).
- Hard TPM reject independent of inflight (composed predicate handles ordering).

## Related

- Parity plan: `/home/stackops/.claude/plans/curried-moseying-pelican.md`
- G4 memory: `[[project_g4_health_prober_state]]` (registry-home pattern reused)
- Two-worktree rule: `[[project_agw_two_worktrees]]` (source from `~/agentgateway`)
- Why health+retry both required: `[[project_agw_priority_group_not_failover]]`
