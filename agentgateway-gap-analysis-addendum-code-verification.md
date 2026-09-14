# Gap Analysis — Code-Level Verification Addendum

**Date:** 2026-06-25
**Purpose:** Continue the Kong-fork ↔ agentgateway gap study by *verifying the prior reports against the actual source* (both repos), not feature summaries. This addendum **confirms** the headline gaps but **corrects several specifics** that change the implementation plan's risk and effort.
**Companions:** `agentgateway-vs-kong-ai-proxy-comparison.md`, `agentgateway-gap-closure-implementation-plan.md`.

---

## 0. TL;DR — What changed after reading the code

| # | Finding | Effect on plan |
|---|---|---|
| 1 | **agentgateway's HTTP retry exists but is *same-endpoint only*** — the backend is selected once *before* the retry loop and reused. Retries never re-run selection. | Confirms the failover gap is real; retry does **not** partially close it. No estimate change, but removes a "maybe it's already there" risk. |
| 2 | **A background-task pattern *does* already exist** — an on-demand, event-driven eviction worker (`tokio::spawn` + min-heap on un-eviction deadlines) in `loadbalancer.rs`. | Plan said "no background-task pattern exists." **Partly wrong.** WS-3.2 active prober and WS-1.4 Prom poller can model on this. **Shaves risk** off Phase 3.2 / 1.4. |
| 3 | **Kong's sticky has a fully *stateless* mode (Path A, `sticky_ttl=0`)** — deterministic weighted consistent-hash over the eligible set, **zero shared store**. | A large slice of WS-2 (Sticky) is achievable in **Tier A with no SharedStore at all**. **Cheaper sticky.** |
| 4 | **Every Kong capacity/sticky/health/cooldown path is fail-open** and falls back to per-pod SHM. They are explicitly "optimizations, not correctness requirements." | Validates Tier A (per-pod) as a *legitimate parity posture*, not a compromise — Kong itself degrades to per-pod under Redis failure. |
| 5 | **agentgateway already does token *reserve→true-up*** via `amend_tokens()` (post-response, async) on the remote-RLS path, reading real usage. | WS-1.3 TPM "true-up at completion" has a working precedent to reuse. **Shaves risk** off Phase 1.3. |
| 6 | **agentgateway's outlier detection is richer than the comparison credited** — `restore_health`, `consecutive_failures` threshold, `health_threshold`, multiplicative backoff `base × (times_ejected+1)`. | The cooldown *state* gap is narrower than stated; the real gaps are (a) cross-instance, (b) status-code-set trigger, (c) **active** probing. |
| 7 | **`EndpointWithInfo` already carries a `capacity: u32`** — but it's a *sampling weight*, not an enforced gate. | WS-1.5 eligibility filter has a field to hang capacity state on, but must add real enforcement (no semaphore/permit exists anywhere). |

Net: **headline tiers/totals stand.** Two phases get *less* risky (3.2, 1.4, 1.3 reuse existing patterns); one feature (sticky) gets a genuinely cheaper Tier-A path. No estimate goes *up*.

---

## 1. agentgateway — verified state of the routing/retry/health path

### 1.1 Retry is same-endpoint (does NOT do backend failover)
`http/retry/mod.rs` defines `Policy { attempts, backoff, codes, precondition, condition }` (CEL pre/post conditions). Applied in `proxy/httpproxy.rs` as `for n in 0..attempts`:

- **Backend is selected once *before* the loop and reused for every attempt** — retries hit the **same endpoint**, never re-running selection. This is HTTP resilience, not multi-backend failover.
- Retrying **buffers the body** via `ReplayBody` up to `MAX_BUFFERED_BYTES` (64 KB); larger bodies disable retry.
- The `precondition` CEL exists specifically so streaming/websocket/large requests skip buffering (`request.method == "GET"`-style gates).

➡️ **The Kong-style "walk primary → fallbacks, mark failed with cooldown" behavior is genuinely absent.** Retry does not substitute for it.

### 1.2 LLM failover is route-resolution-time only
`llm/model_router.rs` `VirtualModelRouting` has `Weighted`, `Failover { backend_key }`, `Conditional(CEL)`. The `Failover` arm **returns a single resolved backend immediately** — no cascading fallback list, no mid-stream switchover, no post-failure cooldown. Selection happens once, pre-first-token.

### 1.3 Health/eviction is passive but more capable than reported
`types/loadbalancer.rs` + `http/outlierdetection.rs`:

- `EndpointInfo`: `health: Ewma` (α=0.3), `request_latency: Ewma`, `pending_requests: ActiveCounter` (in-flight via Arc strong-count), `consecutive_failures`, `times_ejected`, `evicted_until: AtomicOption<Instant>`.
- `score() = health / (1 + latency·(1 + pending·0.1))` — load-aware P2C (`select_p2c`).
- Eviction config: `duration`, `restore_health`, `consecutive_failures`, `health_threshold`; **multiplicative backoff** `base × (times_ejected+1)`; OR-triggered.
- **An eviction worker already runs in the background** — spawned on first eviction event, a `tokio::spawn` task driving a `BinaryHeap<UnevictEntry>` min-heap keyed on un-eviction deadline, waking at the earliest `until`. It is **event-driven, not periodic**, and **purely reactive** (no synthetic probes).

➡️ **Correction to the implementation plan's Appendix** ("No active prober and *no background-task pattern exists yet*"): a spawn+timer-driven worker pattern **does** exist and is the right model to copy for the active prober (WS-3.2) and Prom poller (WS-1.4). What's missing is the *periodic* + *synthetic-probe* + *cross-instance* dimensions, not the spawn scaffolding.

### 1.4 No distributed state; remote-RLS is the only cross-instance hook (CONFIRMED)
- Zero hits for `redis`/`fred`/`memcache`/distributed KV in source (the one "redis" is a hardcoded example proxy *target name* in `tcpproxy.rs`). Postgres appears only for access-log persistence.
- `http/sessionpersistence.rs` is a **stateless encrypted cookie** (`SessionState::encode` → AES-256-GCM or base64; `HTTPSessionState { backend: SocketAddr }`). Not a server-side store.
- `http/remoteratelimit.rs` is the **sole** cross-instance coordinator (Envoy **RLS v3** gRPC, `FailOpen`/`FailClosed`).

### 1.5 Rate-limit ≠ capacity gate (CONFIRMED) — but the true-up half exists
- `localratelimit.rs` is a per-instance atomic **fixed-window token bucket** (`available/refill_at`), not a sliding-window TPM gate.
- **No concurrency gate anywhere** — no `Semaphore`/permit/`max_inflight`. `pending_requests` is observed for scoring only, never enforced.
- **But** the remote-RLS path already does **reserve→true-up**: request phase sends input/zero token cost; after the response, `amend_tokens(input_mismatch + output, …)` reconciles against *real* usage (async `tokio::spawn`). This is exactly the accounting shape WS-1.3 TPM needs — reusable precedent.
- `EndpointWithInfo.capacity: u32` exists but feeds **weighted sampling**, not admission control.

---

## 2. Kong fork — verified internals (refinements beyond the comparison)

### 2.1 Capacity (`capacity.lua` + strategies)
- Registry of strategies, **AND-combined** (`check`); hooks `on_dispatch`/`on_complete`. **All fail-open** (Redis/SHM errors log + allow).
- **concurrent**: atomic ±1 counter, key `aibal:cap:concurrent:<scope>:<provider>|<model>|<url>`, `LEASE_TTL=600s` refreshed on each incr.
- **tpm**: **two-bucket fixed window with linear decay** (`sum = curr + prev·(1−frac)`), not a true sliding window; axes `all`/`in`/`out`; `BUCKET_TTL=180s`; admission estimate = `prompt_chars / chars_per_token` (default **4.0**), recursively walking Anthropic `tool_use`/`tool_result`; **true-up** from real usage at `on_complete`, **full refund** if `ctx.served == false`.
- **prometheus**: read-only PromQL scalar, gate when `value ≥ max_value`, **fail-open on cold/stale** (`max_age`, default 30s) and on any scrape error. Poller is worker-0, one self-rescheduling timer per `(url,query,auth)` (SHA1-keyed, generation kill-switch, config-fingerprint to avoid timer storms), `poll_interval` default 5s.
- Scope `route` vs `global`; store `shm` (per-pod) vs `redis` (`SETEX`/`INCR`/`DECR`/`EXPIRE`, db-scoped pool, `get_reused_times` to skip AUTH/SELECT).

### 2.2 Sticky (`balance.lua` + `sticky_store.lua`) — **three paths, one is stateless**
- **Path A (`sticky_ttl=0`): stateless** deterministic weighted consistent-hash — `sha1_hex(client_id)` → 48-bit int → `h % total_weight`, walk targets subtracting weights. **No store, no rotation.** Same client → same target as long as the eligible set is stable.
- **Path B (`sticky_ttl>0`): store-bound sliding TTL** — read binding (SHM/Redis `SETEX`), if bound target still eligible refresh TTL & reuse, else `weighted_random` + store. Default TTL 300s (matches prompt-cache TTL); also supports ~1800s.
- **Path C**: `sticky_by="none"` → straight WRR. Fallback order everywhere: **sticky → weighted → least-loaded**.
- Client id from `consumer`/`header`/`cookie`/`ip` (+composed `consumer+ip` etc.), SHA1-masked. Missing component degrades silently.

➡️ **Plan impact:** WS-2.1/2.2 can ship **Path A first with zero SharedStore** — a deterministic hash-mod picker over the live endpoint set in `loadbalancer.rs`. That delivers api-key→backend affinity in **Tier A** cheaply; Path B (TTL rebinding to survive set changes) is the only part needing the store.

### 2.3 Failover & health (`failover.lua`, `target_pool.lua`, `health-check.lua`)
- **Cooldown**: SHM/Redis key `ai_fo:<plugin>:<provider>:<model>:<url>` = 1 with TTL `failover_cooldown` (default 180s), set when response status ∈ `failover_on` (default `429,500,502,503,504`) or on TCP failure. `failover_cooldown=0` disables persistence (retry primary every request).
- **Eligibility funnel** (in order): not-already-tried → not-in-cooldown → health-check healthy → under-capacity. **No mid-stream retry** (gates pre-first-token only).
- **Least-loaded overflow**: when all eligible are at capacity, pick lowest `current_count` **from the pre-capacity-filter snapshot**, tiebreak by weight, flag `_soft_degrade`.
- **Active health probe**: worker-0 timer, **30s interval**, **90s status TTL**, tiny chat ping (`max_tokens:10`; Anthropic omits `max_completion_tokens`, uses 10 not 1 because Anthropic reserves against TPM at admission), generation kill-switch + config-fingerprint dedup, status keyed per `(provider,model,url)` (shared across routes). 5xx/429 from a reachable endpoint ⇒ unhealthy; 4xx/2xx ⇒ healthy (proves reachability).

---

## 3. Revised guidance for the implementation plan

1. **Keep Tier A as the default recommendation — and state it's *parity*, not a compromise.** Kong's own capacity/sticky/health are fail-open and fall back to per-pod SHM; a per-instance agentgateway implementation matches Kong's degraded-mode behavior, which is the common operating mode anyway.
2. **Reprioritize sticky.** Split WS-2 into **2a Path-A stateless consistent-hash (Tier A, no store, ~2–3d)** and **2b Path-B store-bound TTL rebinding (Tier B)**. Deliver 2a early — it's a near-free affinity win.
3. **Downgrade the "no background-task pattern" risk.** The eviction worker is the template; WS-3.2/WS-1.4 reuse its `spawn`+deadline-heap shape. Adjust Phase-3.2 confidence from 🟡 toward 🟢 for the *scaffolding* (the GPU-probe-load risk remains).
4. **Reuse `amend_tokens` for TPM true-up.** WS-1.3's "true-up from response/stream usage" is already implemented for RLS; lift the same real-usage extraction. Lowers Phase-1.3 risk.
5. **Be explicit that retry ≠ failover.** When scoping WS-3.1, note agentgateway's retry loop is single-backend; cross-backend cooldown failover is net-new and cannot be faked by raising `attempts`.
6. **Frame the cooldown work precisely.** Don't rebuild eviction state — extend the existing `evicted_until`/outlier machinery with (a) a `failover_on` status-code-set trigger and (b) optional cross-instance backing, rather than a greenfield state machine.

*No totals change. The corrections move three sub-tasks toward lower risk and carve out a cheaper Tier-A sticky path; the Tier A ≈ 45–65d / Tier B ≈ 72–104d envelope from the implementation plan holds.*

---

## Appendix — verification sources

**agentgateway** (`crates/agentgateway/src/`): `http/retry/{mod,body}.rs`, `proxy/httpproxy.rs` (retry loop, single pre-loop backend select; `amend_tokens` post-response), `llm/model_router.rs` (`VirtualModelRouting::Failover`), `types/loadbalancer.rs` (`EndpointInfo`, `score`, `select_p2c`, eviction worker spawn + un-eviction heap, `capacity: u32`), `http/outlierdetection.rs` (`restore_health`/`consecutive_failures`/`health_threshold`/backoff), `http/{localratelimit,remoteratelimit,sessionpersistence}.rs`.

**Kong fork** (`kong/llm/`): `plugin/capacity.lua`, `capacity_store.lua`, `capacity_strategies/{concurrent,tpm,prometheus}.lua`, `prom_poller.lua`, `sticky_store.lua`, `target_pool.lua`, `plugin/shared-filters/{balance,failover,health-check,capacity_release}.lua`, `schemas/init.lua` (config knobs).
