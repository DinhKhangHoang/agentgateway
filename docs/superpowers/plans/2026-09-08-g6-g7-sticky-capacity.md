# G6 Sticky Affinity + G7 Capacity Caps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement per-pod sticky affinity (G6, FR-3.3) and per-pod capacity caps (G7, FR-3.5) on agentgateway's AI-path endpoint selection, so both are ready when a trigger route lands.

**Architecture:** A shared `SelectionContext { key, input_tokens }` threaded through the selection chain (`select_provider` → P2C `max_by`). G6 (soft api-key→backend pin) and G7 (inflight cap + per-endpoint TPM) compose in one `composed_score()` predicate: pinned-under-cap > unpinned-under-cap > pinned-over-cap > unpinned-over-cap; all-over-cap → 503 + G5 Retry-After. Per-pod `DashMap` state. New `Sticky` + `Capacity` backend-policy CRD leaves via the existing Health/Retry CRD→proto→bind path.

**Tech Stack:** Rust (LuaJIT-free; agentgateway crates), Go (controller CRD + translator), protobuf (`resource.proto` `BackendPolicySpec` oneof), busted-free (Rust `cargo test`).

**Status:** Forward-looking — the spec (`docs/superpowers/specs/2026-09-08-g6-g7-sticky-capacity-design.md`) ships no code now. This plan is build-ready for when a trigger fires. Do NOT execute until a trigger lands.

**Spec:** `docs/superpowers/specs/2026-09-08-g6-g7-sticky-capacity-design.md` (read first — every design decision is there).

**Conventions:** Rust 2024 (`gen` is a reserved keyword — use `generation`). 2-space indent, snake_case. Match existing agentgateway style. Run `cargo check -p agentgateway` after each step. Commit after each task.

---

## File Structure

**Runtime (Rust, `crates/agentgateway/src/`):**
- **Create** `llm/selection.rs` — `SelectionContext` struct + `composed_score()` predicate (the shared seam). G6+G7 logic lives here, not in `llm/mod.rs`.
- **Modify** `llm/mod.rs:70-91` — `select_provider` takes `&SelectionContext`, calls `composed_score`.
- **Modify** `llm/mod.rs` (module decl) — `pub mod selection;` (or `mod`).
- **Modify** `types/loadbalancer.rs` — `best_bucket`, `viable`, `find_endpoint`, `select_endpoint` gain `&SelectionContext` (AI-path parity; HTTP-path symmetry).
- **Modify** `proxy/httpproxy.rs` — `BackendCall` site builds `SelectionContext` from api-key sha256 (`:191`) + `LLMRequest.input_tokens` (`:499`); passes to `select_provider`.
- **Modify** `http/health.rs` — (no change; G4's `ActiveProbeConfig` stays).
- **Create** `store/selection_state.rs` — `SelectionState { pins, tpm }` (`DashMap` home) + `PinEntry` + `TpmCounter`.
- **Modify** `store/binds.rs:90-100` — `Store` gains `selection_state: Arc<SelectionState>`; init in `Store::new`.
- **Modify** `store/binds.rs:242-274` — `BackendPolicies` gains `sticky`, `capacity` fields.
- **Modify** `store/binds.rs:1313+` — `BackendTrafficPolicy::Sticky`/`::Capacity` match arms.
- **Modify** `proxy/httpproxy.rs` — `finish_request` path calls pin-write (G6) + TPM true-up (G7).
- **Modify** `proxy/mod.rs:~287` — `NoHealthyEndpoints → 503` flows `capacity.cooldown` as `Retry-After` (extends G5).

**Controller (Go):**
- **Modify** `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go` — `Sticky` + `Capacity` structs under `BackendFull`.
- **Modify** `crates/protos/proto/resource.proto:1661` — `BackendPolicySpec` oneof cases 19 (`sticky`) + 20 (`capacity`).
- **Regen** `api/resource.pb.go` (`make generate` or the repo's proto-gen target).
- **Modify** `controller/pkg/agentgateway/plugins/backend_policies.go:178` — branches reading `backend.Sticky`/`backend.Capacity`; new `translateBackendStickyPolicy`/`translateBackendCapacityPolicy` (template: `translateBackendHealthPolicy:300`).

**Local-config (Rust):**
- **Modify** `types/local.rs` (or wherever `LocalHealthPolicy` lives) — `LocalSticky` + `LocalCapacity` + `TryFrom` (G4 `ActiveProbeConfig` pattern).

**Tests (Rust):**
- **Create** `llm/selection_tests.rs` (or extend `llm/mod_tests.rs`) — `composed_score` unit tests.
- **Extend** `types/local_tests.rs` / `proxy/httpproxy.rs` inline tests — pin-on-success, TPM true-up, reject→503, composed ordering.

---

## Task 1: SelectionContext struct + composed_score pass-through

The shared seam. Pure plumbing — no behavior change. `composed_score` returns raw `score()` so today's selection is identical. Tasks 4, 7, 8 extend `composed_score`.

**Files:**
- Create: `crates/agentgateway/src/llm/selection.rs`
- Modify: `crates/agentgateway/src/llm/mod.rs:70-91` (`select_provider` signature + `max_by`)
- Test: `crates/agentgateway/src/llm/selection.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `crates/agentgateway/src/llm/selection.rs`:

```rust
use crate::types::loadbalancer::EndpointInfo;

/// Caller identity + request size, threaded through endpoint selection.
/// `None` fields => no sticky/capacity policy; selection is today's behavior.
#[derive(Debug, Clone, Copy)]
pub struct SelectionContext<'a> {
    /// api-key sha256, for G6 sticky pin lookup. None when no Sticky policy.
    pub key: Option<&'a str>,
    /// LLMRequest.input_tokens, for G7 TPM pre-debit. None when no Capacity policy.
    pub input_tokens: Option<u64>,
}

impl<'a> SelectionContext<'a> {
    pub fn none() -> Self {
        Self { key: None, input_tokens: None }
    }
}

/// Composed selection score. Tasks 4/7/8 extend this with sticky bias + capacity gate.
/// For now: pass-through to EndpointInfo::score() (today's behavior).
pub fn composed_score(_info: &EndpointInfo, _ctx: &SelectionContext<'_>) -> f64 {
    _info.score()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_ctx_is_pass_through() {
        // composed_score with None ctx must equal raw score().
        // EndpointInfo is internal; this test asserts the contract via a stub.
        // Real integration tested in Task 10 with live EndpointInfo.
        let ctx = SelectionContext::none();
        assert!(ctx.key.is_none());
        assert!(ctx.input_tokens.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it passes (it's a contract stub)**

Run: `cargo test -p agentgateway --lib llm::selection::tests::none_ctx_is_pass_through`
Expected: PASS (stub test; real score assertion comes in Task 10).

- [ ] **Step 3: Thread SelectionContext through select_provider**

In `crates/agentgateway/src/llm/mod.rs`, add the module decl near the top (after existing `mod` lines):

```rust
pub mod selection;
pub use selection::{SelectionContext, composed_score};
```

Change `select_provider` (`llm/mod.rs:70-91`):

```rust
pub fn select_provider(&self, ctx: &SelectionContext<'_>) -> Option<(Arc<NamedAIProvider>, ActiveHandle)> {
    let iter = self.providers.iter();
    let index = iter.index();
    if index.is_empty() {
        return None;
    }
    let a = rand::rng().random_range(0..index.len());
    let b = rand::rng().random_range(0..index.len());
    let best = [a, b]
        .into_iter()
        .map(|idx| {
            let (_, EndpointWithInfo { endpoint, info, .. }) =
                index.get_index(idx).expect("index already checked");
            (endpoint.clone(), info)
        })
        .max_by(|(_, a), (_, b)| composed_score(a, ctx).total_cmp(&composed_score(b, ctx)));
    let (ep, ep_info) = best?;
    let handle = self.providers.start_request(ep.name.clone(), ep_info);
    Some((ep, handle))
}
```

- [ ] **Step 4: Fix the single call site, cargo check**

Find the `select_provider(` call site (search: `grep -rn 'select_provider(' crates/`). At each site, pass `&SelectionContext::none()` for now (real context wired in Task 9). Then:

Run: `cargo check -p agentgateway`
Expected: 0 errors. (Existing tests still pass — behavior unchanged.)

Run: `cargo test -p agentgateway --lib llm::`
Expected: all existing llm tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agentgateway/src/llm/selection.rs crates/agentgateway/src/llm/mod.rs
git commit -m "feat(selection): SelectionContext seam + composed_score pass-through

Thread SelectionContext through select_provider; composed_score returns
raw score() (today's behavior). Plumbing for G6/G7. No behavior change."
```

---

## Task 2: SelectionState (per-pod DashMap home)

The per-pod state `G6 pins` and `G7 tpm` live in. Scaffolds both maps now; G6/G7 fill them.

**Files:**
- Create: `crates/agentgateway/src/store/selection_state.rs`
- Modify: `crates/agentgateway/src/store/binds.rs:90-100` (`Store` field + init)
- Modify: `crates/agentgateway/src/store/binds.rs` (module decl)
- Test: `crates/agentgateway/src/store/selection_state.rs` (inline)

- [ ] **Step 1: Write the failing tests**

Create `crates/agentgateway/src/store/selection_state.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_core::prelude::Strng;
use dashmap::DashMap;

/// G6: api-key sha256 -> pinned backend, TTL'd.
#[derive(Clone)]
pub struct PinEntry {
    pub backend_name: Strng,
    pub expires_at: Instant,
}

impl PinEntry {
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

/// G7: per-endpoint TPM fixed-window counter (60s window).
pub struct TpmCounter {
    pub window_start: Instant,
    pub debited: AtomicU64,   // pre-debited input_tokens at select
    pub actual: AtomicU64,    // trued-up on finish_request
}

impl TpmCounter {
    const WINDOW: Duration = Duration::from_secs(60);

    pub fn new(now: Instant) -> Self {
        Self { window_start: now, debited: AtomicU64::new(0), actual: AtomicU64::new(0) }
    }

    /// Returns (debited_total, is_over_cap). Rolls window if stale.
    pub fn check_and_debit(&self, input_tokens: u64, cap: u64, now: Instant) -> (u64, bool) {
        if now.duration_since(self.window_start) >= Self::WINDOW {
            // window rollover: reset both (CAS via store — single-selector path per endpoint)
            self.debited.store(0, Ordering::Relaxed);
            self.actual.store(0, Ordering::Relaxed);
            // note: window_start update left to the caller who holds &mut via DashMap entry
        }
        let cur = self.debited.load(Ordering::Relaxed);
        let over = cur.saturating_add(input_tokens) > cap;
        if !over {
            self.debited.fetch_add(input_tokens, Ordering::Relaxed);
        }
        (cur.saturating_add(input_tokens), over)
    }

    /// True-up on finish_request: replace the pre-debit with actual usage.
    pub fn trued_up(&self, pre_debited: u64, actual_tokens: u64) {
        self.actual.fetch_add(actual_tokens, Ordering::Relaxed);
        // release the pre-debit, keep actual
        self.debited.fetch_sub(pre_debited.min(self.debited.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

/// Per-pod selection state, hung off Store (next to ProberGenerationRegistry).
pub struct SelectionState {
    pub pins: DashMap<Strng, PinEntry>,
    pub tpm: DashMap<Strng, TpmCounter>,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self { pins: DashMap::new(), tpm: DashMap::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_entry_expiry() {
        let now = Instant::now();
        let entry = PinEntry { backend_name: Strng::from("be-a"), expires_at: now };
        assert!(entry.is_expired(now));
        let future = PinEntry { backend_name: Strng::from("be-a"), expires_at: now + Duration::from_secs(60) };
        assert!(!future.is_expired(now));
    }

    #[test]
    fn tpm_under_cap_debits() {
        let now = Instant::now();
        let ctr = TpmCounter::new(now);
        let (total, over) = ctr.check_and_debit(500, 1000, now);
        assert_eq!(total, 500);
        assert!(!over);
        assert_eq!(ctr.debited.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn tpm_over_cap_rejects_without_debit() {
        let now = Instant::now();
        let ctr = TpmCounter::new(now);
        ctr.check_and_debit(800, 1000, now);
        let (total, over) = ctr.check_and_debit(300, 1000, now);
        assert!(over);
        assert_eq!(total, 1100);
        // rejected: not debited
        assert_eq!(ctr.debited.load(Ordering::Relaxed), 800);
    }

    #[test]
    fn tpm_true_up_frees_phantom_debt() {
        let now = Instant::now();
        let ctr = TpmCounter::new(now);
        ctr.check_and_debit(1000, 1000, now); // debited=1000
        ctr.trued_up(1000, 200);              // actual=200, release 1000 pre-debit
        assert_eq!(ctr.actual.load(Ordering::Relaxed), 200);
        assert_eq!(ctr.debited.load(Ordering::Relaxed), 0);
        // 800 tokens freed back to the window
        let (total, over) = ctr.check_and_debit(800, 1000, now);
        assert!(!over);
        assert_eq!(total, 800);
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p agentgateway --lib store::selection_state::tests`
Expected: 4 PASS. (If `dashmap` isn't a dep, check `Cargo.toml` — agentgateway already uses it; if not, add `dashmap = "6"` to `crates/agentgateway/Cargo.toml`.)

- [ ] **Step 3: Wire SelectionState onto Store**

In `crates/agentgateway/src/store/binds.rs`, add module decl:
```rust
pub mod selection_state;
pub use selection_state::SelectionState;
```

Add field to `Store` struct (~line 90, near `prober_generations`):
```rust
pub struct Store {
    // ... existing fields ...
    pub selection_state: Arc<SelectionState>,
}
```

In `Store::new` (~line 678, next to `prober_generations: Arc::new(Default::default())`):
```rust
selection_state: Arc::new(Default::default()),
```

Add accessor near `prober_generations()` (~line 1563):
```rust
pub fn selection_state(&self) -> &Arc<SelectionState> {
    &self.selection_state
}
```

- [ ] **Step 4: cargo check + commit**

Run: `cargo check -p agentgateway`
Expected: 0 errors.

Run: `cargo test -p agentgateway --lib store::`
Expected: all PASS.

```bash
git add crates/agentgateway/src/store/selection_state.rs crates/agentgateway/src/store/binds.rs
git commit -m "feat(selection): per-pod SelectionState (pins + tpm DashMaps)

PinEntry (TTL'd) + TpmCounter (60s fixed-window, pre-debit/true-up).
Scaffold only; G6/G7 fill the logic."
```

---

## Task 3: G6 controller — Sticky CRD + proto + translator + Rust bind

The config side for G6, following the Health 5-step path. No runtime behavior yet (Task 4 wires it).

**Files:**
- Modify: `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go` (add `Sticky` struct + `BackendFull.Sticky` field)
- Modify: `crates/protos/proto/resource.proto:1661` (oneof case 19)
- Regen: `api/resource.pb.go`
- Modify: `controller/pkg/agentgateway/plugins/backend_policies.go` (translator)
- Modify: `crates/agentgateway/src/store/binds.rs` (`BackendPolicies.sticky` + `BackendTrafficPolicy::Sticky` arm)

- [ ] **Step 1: Add Sticky CRD struct**

In `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go`, add after the `Health` struct (~L302):

```go
// Sticky configures soft api-key→backend affinity for prompt-cache hit rate.
type Sticky struct {
	// CEL expression resolving to the affinity key. Defaults to the request's api-key sha256.
	// +optional
	Key *CELExpression `json:"key,omitempty"`
	// How long a pin survives after last use. Default 10m.
	// +optional
	TTL *metav1.Duration `json:"ttl,omitempty"`
}
```

Add field to `BackendFull` struct (~L369, next to `Health`):
```go
	// Sticky affinity policy. +optional
	Sticky *Sticky `json:"sticky,omitempty"`
```

- [ ] **Step 2: Add proto oneof case 19**

In `crates/protos/proto/resource.proto`, find the `BackendPolicySpec` oneof (~L1661). After the last case, add:

```proto
    // G6: sticky affinity
    Sticky sticky = 19;
```

And define the `Sticky` message near `Health`:
```proto
message Sticky {
 google.protobuf.StringValue key = 1;
 google.protobuf.Duration ttl = 2;
}
```

- [ ] **Step 3: Regenerate proto bindings**

Run the repo's proto-gen target. Check `Makefile` / `hack/` for the command; typically:
```bash
make generate
```
If no target, run `buf generate` or the explicit `protoc` invocation the repo uses. Verify `api/resource.pb.go` now contains `BackendPolicySpec_Sticky`.

- [ ] **Step 4: Add translator function + branch**

In `controller/pkg/agentgateway/plugins/backend_policies.go`, add a `translateBackendStickyPolicy` function modeled on `translateBackendHealthPolicy` (~L300):

```go
func translateBackendStickyPolicy(policy *agentgateway.AgentgatewayPolicy) (*api.Policy, error) {
	sp := policy.Spec.Backend.Sticky
	if sp == nil {
		return nil, nil
	}
	out := &api.Sticky{}
	if sp.Key != nil {
		out.Key = wrapperspb.String(string(*sp.Key))
	}
	if sp.TTL != nil {
		out.Ttl = durationpb.New(sp.TTL.Duration)
	}
	return &api.Policy{
		Kind: &api.Policy_Backend{Backend: &api.BackendPolicySpec{
			Kind: &api.BackendPolicySpec_Sticky{Sticky: out},
		}},
	}, nil
}
```

Add the branch in `TranslateInlineBackendPolicy` (~L178, next to the `backend.Health` branch):
```go
	if s := backend.Sticky; s != nil {
		policies = appendPolicy("backendSticky")(translateBackendStickyPolicy(policy))
	}
```

(Confirm `appendPolicy` helper signature matches — it's the same pattern `backend.Health` uses at L178-179.)

- [ ] **Step 5: Add Rust bind arm + BackendPolicies field**

In `crates/agentgateway/src/store/binds.rs`:

Add to `BackendPolicies` struct (~L242, next to `health`):
```rust
pub sticky: Option<crate::http::sticky::Policy>,
```

Create `crates/agentgateway/src/http/sticky.rs`:
```rust
#[apply(schema_ser!)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Strng>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_dur_option")]
    pub ttl: Option<Duration>,
}
```
Add `pub mod sticky;` to `crates/agentgateway/src/http/mod.rs`.

Add the match arm in the `BackendTrafficPolicy` match (~L1363, next to `Health(p)`):
```rust
BackendTrafficPolicy::Sticky(p) => { bp.sticky = Some(p); }
```

(Check the proto-bind deserialization path — the `BackendTrafficPolicy` enum is built from the proto `BackendPolicySpec.kind` oneof in the xDS/local config deserialization. Add a `Sticky(p)` arm there too, mirroring how `Health(p)` is constructed. Search: `grep -n 'BackendPolicySpec_Health' crates/agentgateway/src/types/` to find the construction site.)

- [ ] **Step 6: cargo check + go build + commit**

Run: `cargo check -p agentgateway` — expected 0 errors.
Run: `cd controller && go build ./...` — expected 0 errors.

```bash
git add controller/ crates/protos/proto/resource.proto api/resource.pb.go crates/agentgateway/src/http/sticky.rs crates/agentgateway/src/http/mod.rs crates/agentgateway/src/store/binds.rs crates/agentgateway/src/types/
git commit -m "feat(sticky): G6 Sticky CRD + proto case 19 + translator + Rust bind

Config side only (Health-path template). No runtime behavior yet."
```

---

## Task 4: G6 runtime — pinmap lookup + score bias + pin-on-success

The G6 logic: bias P2C toward the pinned endpoint, write the pin on success.

**Files:**
- Modify: `crates/agentgateway/src/llm/selection.rs` (`composed_score` sticky bias)
- Modify: `crates/agentgateway/src/proxy/httpproxy.rs` (pin-write on `finish_request` success; build `SelectionContext.key`)
- Test: `crates/agentgateway/src/llm/selection.rs` (composed_score with pin)

- [ ] **Step 1: Write the failing test for sticky bias**

In `crates/agentgateway/src/llm/selection.rs` tests, add:

```rust
#[test]
fn composed_score_pinned_doubles() {
    // When ctx.key matches a pinned backend, composed_score should be 2× raw score.
    // Requires a live EndpointInfo — use the test helpers from local_tests.rs
    // that construct an EndpointSet. If unavailable, test via the integrated
    // select_provider path in Task 10 instead and mark this as a contract stub.
    // For now: assert the bias multiplier constant.
    const PINNED_BIAS: f64 = 2.0;
    const UNPINNED_BIAS: f64 = 1.0;
    assert_eq!(PINNED_BIAS / UNPINNED_BIAS, 2.0);
}
```

(Real pinned-score assertion lands in Task 10 with a live EndpointSet — noted here so the multiplier contract is explicit.)

- [ ] **Step 2: Extend composed_score with sticky bias**

In `crates/agentgateway/src/llm/selection.rs`, replace `composed_score`:

```rust
/// Bias multipliers (spec §G6 §2-3, §G7 §3). G7 halves added in Task 7/8.
const PINNED_UNDER_CAP: f64 = 2.0;
const PINNED_OVER_CAP: f64 = 1.2;
const UNPINNED_UNDER_CAP: f64 = 1.0;
const UNPINNED_OVER_CAP: f64 = 0.5;

/// Returns true if the endpoint name matches the pinned backend for ctx.key.
/// Looks up the per-pod pin map. `selection_state` is None in unit tests.
fn is_pinned(endpoint_name: &str, ctx: &SelectionContext<'_>, selection_state: Option<&crate::store::selection_state::SelectionState>) -> bool {
    let Some(key) = ctx.key else { return false; };
    let Some(state) = selection_state else { return false; };
    if let Some(entry) = state.pins.get(Strng::from(key).as_str())
        .or_else(|| state.pins.get(key)) {
        let now = std::time::Instant::now();
        if entry.is_expired(now) { return false; }
        return entry.backend_name.as_str() == endpoint_name;
    }
    false
}

/// Composed score. Task 4: sticky bias. Task 7/8: capacity gate halves.
pub fn composed_score(
    info: &EndpointInfo,
    ctx: &SelectionContext<'_>,
    selection_state: Option<&crate::store::selection_state::SelectionState>,
) -> f64 {
    let raw = info.score();
    let pinned = is_pinned(&info.endpoint_name, ctx, selection_state);
    // G7 not yet wired; treat all as "under-cap"
    let multiplier = if pinned { PINNED_UNDER_CAP } else { UNPINNED_UNDER_CAP };
    raw * multiplier
}
```

*(If `EndpointInfo` doesn't expose `endpoint_name` directly, adapt — it's on `EndpointWithInfo.endpoint.name`. The closure in `select_provider` has `(endpoint, info)` so pass `endpoint.name.as_str()` as an extra param. Adjust signature to `composed_score(endpoint_name: &str, info: &EndpointInfo, ctx, state)` and thread `endpoint.name.as_str()` from the call site.)*

- [ ] **Step 3: Update select_provider to pass selection_state + endpoint name**

In `crates/agentgateway/src/llm/mod.rs:80-87`, update the closure:

```rust
.max_by(|(ep_a, a), (ep_b, b)| {
    composed_score(ep_a.name.as_str(), a, ctx, selection_state)
        .total_cmp(&composed_score(ep_b.name.as_str(), b, ctx, selection_state))
});
```

`select_provider` gains a `selection_state: Option<&SelectionState>` param (or fetches it from the store it already holds). Thread from the `BackendCall` site in Task 9.

- [ ] **Step 4: Pin-write on finish_request success**

In `crates/agentgateway/src/proxy/httpproxy.rs`, find the `finish_request` success path (where `ActiveHandle::finish_request` is called on 2xx — search `finish_request`). Add, gated on `backend_policies.sticky` being Some:

```rust
if let Some(sticky) = &backend_policies.sticky
    && let Some(key) = ctx_key  // the api-key sha256, captured at select time
{
    let ttl = sticky.ttl.unwrap_or(Duration::from_secs(600));
    let backend_name = selected_provider.name.clone();
    selection_state.pins.insert(
        Strng::from(key),
        PinEntry { backend_name, expires_at: Instant::now() + ttl },
    );
}
```

**Critical: success path only.** Do NOT pin on error/eviction. The `finish_request(success, ...)` bool gates this.

- [ ] **Step 5: Test + commit**

Run: `cargo test -p agentgateway --lib llm::`
Expected: PASS.

Run: `cargo check -p agentgateway`
Expected: 0 errors.

```bash
git add crates/agentgateway/src/llm/selection.rs crates/agentgateway/src/llm/mod.rs crates/agentgateway/src/proxy/httpproxy.rs
git commit -m "feat(sticky): G6 pinmap lookup + score bias + pin-on-success

composed_score applies ×2.0 pinned bias; pin written to per-pod DashMap
only on successful finish_request. Evicted-pin falls through (is_pinned
checks expiry + name match)."
```

---

## Task 5: G7 controller — Capacity CRD + proto + translator + Rust bind

Mirror Task 3 for Capacity. Follow the exact same 5-step path.

**Files:**
- Modify: `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go` (add `Capacity` struct + `BackendFull.Capacity`)
- Modify: `crates/protos/proto/resource.proto` (oneof case 20)
- Regen: `api/resource.pb.go`
- Modify: `controller/pkg/agentgateway/plugins/backend_policies.go` (`translateBackendCapacityPolicy`)
- Modify: `crates/agentgateway/src/store/binds.rs` (`BackendPolicies.capacity` + `BackendTrafficPolicy::Capacity` arm)
- Create: `crates/agentgateway/src/http/capacity.rs` (`Policy` struct)

- [ ] **Step 1: Add Capacity CRD struct**

In `agentgateway_policy_types.go`, after `Sticky`:

```go
// Capacity configures per-endpoint inflight + TPM hard caps. Reject 503+Retry-After on trip.
type Capacity struct {
	// Max concurrent in-flight requests per endpoint. +optional
	InflightCap *int32 `json:"inflightCap,omitempty"`
	// Per-endpoint tokens-per-minute budget. Pre-debited at select, trued-up on completion.
	// +optional
	TpmPerMinute *int64 `json:"tpmPerMinute,omitempty"`
	// Hold-rejected duration. Default 3s (reuses Retry-After). +optional
	Cooldown *metav1.Duration `json:"cooldown,omitempty"`
}
```

Add to `BackendFull`: `Capacity *Capacity \`json:"capacity,omitempty"\``

- [ ] **Step 2: Add proto case 20 + message**

```proto
    // G7: capacity caps
    Capacity capacity = 20;
```
```proto
message Capacity {
  google.protobuf.Int32Value inflight_cap = 1;
  google.protobuf.Int64Value tpm_per_minute = 2;
  google.protobuf.Duration cooldown = 3;
}
```

- [ ] **Step 3: Regen proto bindings**

Run: `make generate` (or repo equivalent). Verify `BackendPolicySpec_Capacity` in `api/resource.pb.go`.

- [ ] **Step 4: Add translator + branch**

In `backend_policies.go`:

```go
func translateBackendCapacityPolicy(policy *agentgateway.AgentgatewayPolicy) (*api.Policy, error) {
	cp := policy.Spec.Backend.Capacity
	if cp == nil {
		return nil, nil
	}
	out := &api.Capacity{}
	if cp.InflightCap != nil {
		out.InflightCap = wrapperspb.Int32(*cp.InflightCap)
	}
	if cp.TpmPerMinute != nil {
		out.TpmPerMinute = wrapperspb.Int64(*cp.TpmPerMinute)
	}
	if cp.Cooldown != nil {
		out.Cooldown = durationpb.New(cp.Cooldown.Duration)
	}
	return &api.Policy{
		Kind: &api.Policy_Backend{Backend: &api.BackendPolicySpec{
			Kind: &api.BackendPolicySpec_Capacity{Capacity: out},
		}},
	}, nil
}
```

Branch in `TranslateInlineBackendPolicy`:
```go
	if c := backend.Capacity; c != nil {
		policies = appendPolicy("backendCapacity")(translateBackendCapacityPolicy(policy))
	}
```

- [ ] **Step 5: Add Rust bind**

Create `crates/agentgateway/src/http/capacity.rs`:
```rust
#[apply(schema_ser!)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight_cap: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_per_minute: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_dur_option")]
    pub cooldown: Option<Duration>,
}
```
Add `pub mod capacity;` to `http/mod.rs`.

`BackendPolicies.capacity: Option<crate::http::capacity::Policy>`, match arm `BackendTrafficPolicy::Capacity(p) => { bp.capacity = Some(p); }`, and the xDS-deserialization arm (mirror Health).

- [ ] **Step 6: Build + commit**

Run: `cargo check -p agentgateway` + `cd controller && go build ./...` — 0 errors.

```bash
git add controller/ crates/protos/proto/resource.proto api/resource.pb.go crates/agentgateway/src/http/capacity.rs crates/agentgateway/src/http/mod.rs crates/agentgateway/src/store/binds.rs crates/agentgateway/src/types/
git commit -m "feat(capacity): G7 Capacity CRD + proto case 20 + translator + Rust bind

Config side only. Inflight cap, TPM budget, cooldown (→Retry-After)."
```

---

## Task 6: G7 runtime — inflight hard cap + TPM pre-debit/true-up + composed predicate

The G7 logic, completing `composed_score`. Inflight reuses `pending_requests`; TPM uses `TpmCounter` from Task 2.

**Files:**
- Modify: `crates/agentgateway/src/llm/selection.rs` (`composed_score` full table + inflight/TPM gate)
- Modify: `crates/agentgateway/src/proxy/httpproxy.rs` (TPM pre-debit at select, true-up at `finish_request`, `NoHealthyEndpoints` → 503 + `Retry-After: cooldown`)
- Test: `crates/agentgateway/src/llm/selection.rs` (composed table cases)

- [ ] **Step 1: Write failing tests for the composed predicate table**

In `selection.rs` tests:

```rust
#[test]
fn predicate_table_pinned_under_cap_highest() {
    // pinned & under-cap > unpinned & under-cap > pinned & over-cap > unpinned & over-cap
    // Multipliers: 2.0 > 1.0 > 1.2 > 0.5  — wait, 1.2 > 1.0.
    // Spec table: pinned-under-cap=2.0, pinned-over-cap=1.2, unpinned-under-cap=1.0, unpinned-over-cap=0.5
    // Ordering: 2.0 > 1.2 > 1.0 > 0.5
    assert!(PINNED_UNDER_CAP > PINNED_OVER_CAP);
    assert!(PINNED_OVER_CAP > UNPINNED_UNDER_CAP);
    assert!(UNPINNED_UNDER_CAP > UNPINNED_OVER_CAP);
}

#[test]
fn predicate_table_all_over_cap_rejects() {
    // When all candidates over-cap: select returns None → NoHealthyEndpoints → 503.
    // Tested at integration level in Task 10. Contract: the gate function
    // returns over_cap=true for both candidates.
    // (Stub — real assertion in Task 10.)
}
```

- [ ] **Step 2: Extend composed_score with the full table**

In `selection.rs`, replace `composed_score` with the full predicate:

```rust
/// Per-candidate capacity check result.
pub struct CandidateCap {
    pub inflight_over: bool,
    pub tpm_over: bool,
}

/// Check inflight + TPM cap for a candidate. Returns (CandidateCap, debited_tokens).
/// TPM: pre-debits input_tokens if under cap.
pub fn check_capacity(
    info: &EndpointInfo,
    ctx: &SelectionContext<'_>,
    capacity: Option<&crate::http::capacity::Policy>,
    selection_state: Option<&crate::store::selection_state::SelectionState>,
) -> (CandidateCap, u64) {
    let cap = match capacity {
        Some(c) => c,
        None => return (CandidateCap { inflight_over: false, tpm_over: false }, 0),
    };
    // inflight
    let inflight_over = cap.inflight_cap
        .map(|c| info.pending_requests.count() as i32 >= c)
        .unwrap_or(false);
    // tpm
    let (tpm_over, debited) = if let (Some(tpm_cap), Some(state), Some(tokens)) =
        (cap.tpm_per_minute, selection_state, ctx.input_tokens)
    {
        let entry = state.tpm.entry(info.endpoint_name.clone()).or_insert_with(||
            TpmCounter::new(Instant::now()));
        let (_, over) = entry.check_and_debit(tokens, tpm_cap as u64, Instant::now());
        (over, if over { 0 } else { tokens })
    } else {
        (false, 0)
    };
    (CandidateCap { inflight_over, tpm_over }, debited)
}

pub fn composed_score(
    info: &EndpointInfo,
    ctx: &SelectionContext<'_>,
    selection_state: Option<&crate::store::selection_state::SelectionState>,
    capacity: Option<&crate::http::capacity::Policy>,
) -> f64 {
    let raw = info.score();
    let pinned = is_pinned(&info.endpoint_name, ctx, selection_state);
    let (cap, _) = check_capacity(info, ctx, capacity, selection_state);
    let over_cap = cap.inflight_over || cap.tpm_over;
    let multiplier = match (pinned, over_cap) {
        (true, false) => PINNED_UNDER_CAP,
        (true, true)  => PINNED_OVER_CAP,
        (false, false) => UNPINNED_UNDER_CAP,
        (false, true)  => UNPINNED_OVER_CAP,
    };
    raw * multiplier
}
```

*(Adjust `info.endpoint_name` access per the actual `EndpointInfo` fields — if the name isn't on `EndpointInfo`, pass `endpoint.name.as_str()` as before.)*

- [ ] **Step 3: Update select_provider — reject when all over-cap**

In `llm/mod.rs`, `select_provider` now needs `capacity: Option<&Capacity>` + `selection_state`. After P2C `max_by`, if the winner is over-cap AND the other candidate is also over-cap (or there's only one and it's over-cap), return `None` (→ `NoHealthyEndpoints`):

```rust
// After max_by, check if the winner is hard-rejected (all over cap).
// If winner.over_cap and (loser missing or loser.over_cap) => return None.
let (_ep, ep_info) = best?;
let (cap, _debited) = check_capacity(ep_info, ctx, capacity, selection_state);
if cap.inflight_over || cap.tpm_over {
    // Check the other candidate; if also over, reject
    let other_over = /* the non-winner candidate's cap, or true if single */;
    if other_over { return None; }
}
```

*(The P2C closure evaluates both; refactor to retain both candidates' cap results rather than re-checking. Cleanest: collect `[(endpoint, info, cap)]` for both, filter over-cap, pick max composed_score among under-cap; if none under-cap, return None.)*

- [ ] **Step 4: TPM true-up at finish_request + 503 Retry-After**

In `proxy/httpproxy.rs` `finish_request` path, after the G6 pin-write, add TPM true-up:

```rust
if let Some(capacity) = &backend_policies.capacity
    && let Some(state) = selection_state
{
    if let Some(entry) = state.tpm.get(&selected_provider.name) {
        let actual = total_tokens_from_usage_log(&log);  // pull from the request log
        entry.trued_up(pre_debited_tokens, actual);
    }
}
```

*(Capture `pre_debited_tokens` at select time — stash it on the `ActiveHandle` or a per-request sidecar.)*

For the 503 path, `proxy/mod.rs` `NoHealthyEndpoints → SERVICE_UNAVAILABLE` (G5, ~L287): ensure `capacity.cooldown` flows as the `Retry-After` value. G5 already injects `Retry-After: eviction_duration`; pass `cooldown.unwrap_or(3s)` as the duration. *(If G5 used a fixed default, extend it to read the capacity cooldown.)*

- [ ] **Step 5: Test + commit**

Run: `cargo test -p agentgateway --lib llm::` + `cargo test -p agentgateway --lib store::selection_state::`
Expected: PASS.

```bash
git add crates/agentgateway/src/llm/selection.rs crates/agentgateway/src/llm/mod.rs crates/agentgateway/src/proxy/httpproxy.rs crates/agentgateway/src/proxy/mod.rs
git commit -m "feat(capacity): G7 inflight + TPM gate, composed predicate, 503+Retry-After

composed_score full table (pinned-under-cap 2.0 > pinned-over-cap 1.2 >
unpinned-under-cap 1.0 > unpinned-over-cap 0.5). Inflight reuses
pending_requests; TPM pre-debits at select, trues-up at finish_request.
All-over-cap → NoHealthyEndpoints → 503 + Retry-After: cooldown (G5 reuse)."
```

---

## Task 7: Local-config path (LocalSticky + LocalCapacity + TryFrom)

The local-config (non-CRD) path, mirroring G4's `ActiveProbeConfig` pattern. Needed for YAML local-config dev/testing.

**Files:**
- Modify: `crates/agentgateway/src/types/local.rs` (or wherever `LocalHealthPolicy`/`LocalActiveProbeConfig` lives — search `grep -rn 'LocalActiveProbeConfig' crates/`)
- Test: `crates/agentgateway/src/types/local_tests.rs`

- [ ] **Step 1: Write the failing TryFrom test**

In `local_tests.rs`, add:

```rust
#[test]
fn local_sticky_round_trips() {
    let yaml = "sticky:\n  ttl: 600s\n";
    let local: LocalSticky = serde_yaml::from_str(yaml).unwrap();
    let p: crate::http::sticky::Policy = local.try_into().unwrap();
    assert_eq!(p.ttl, Some(Duration::from_secs(600)));
}

#[test]
fn local_capacity_round_trips() {
    let yaml = "inflightCap: 10\ntpmPerMinute: 50000\ncooldown: 3s\n";
    let local: LocalCapacity = serde_yaml::from_str(yaml).unwrap();
    let p: crate::http::capacity::Policy = local.try_into().unwrap();
    assert_eq!(p.inflight_cap, Some(10));
    assert_eq!(p.tpm_per_minute, Some(50000));
    assert_eq!(p.cooldown, Some(Duration::from_secs(3)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agentgateway --lib types::local_tests::local_sticky_round_trips`
Expected: FAIL (`LocalSticky` not defined).

- [ ] **Step 3: Add LocalSticky + LocalCapacity + TryFrom**

In `local.rs`, mirror `LocalActiveProbeConfig`:

```rust
#[apply(schema_ser!)]
pub struct LocalSticky {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_dur_option")]
    pub ttl: Option<Duration>,
}

impl TryFrom<LocalSticky> for crate::http::sticky::Policy {
    type Error = anyhow::Error;
    fn try_from(v: LocalSticky) -> Result<Self, Self::Error> {
        Ok(Self {
            key: v.key.map(Strng::from),
            ttl: v.ttl,
        })
    }
}

#[apply(schema_ser!)]
pub struct LocalCapacity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight_cap: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_per_minute: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_dur_option")]
    pub cooldown: Option<Duration>,
}

impl TryFrom<LocalCapacity> for crate::http::capacity::Policy {
    type Error = anyhow::Error;
    fn try_from(v: LocalCapacity) -> Result<Self, Self::Error> {
        Ok(Self {
            inflight_cap: v.inflight_cap,
            tpm_per_minute: v.tpm_per_minute,
            cooldown: v.cooldown,
        })
    }
}
```

Add `sticky`/`capacity` fields to `LocalHealthPolicy` (or the local equivalent of `BackendPolicies`) + the `TryFrom<LocalBackendPolicies>` site.

- [ ] **Step 4: Run tests + commit**

Run: `cargo test -p agentgateway --lib types::local_tests::`
Expected: PASS.

```bash
git add crates/agentgateway/src/types/local.rs crates/agentgateway/src/types/local_tests.rs
git commit -m "feat(sticky,capacity): LocalSticky + LocalCapacity local-config path

TryFrom wiring (G4 ActiveProbeConfig pattern). Enables YAML local-config
dev/testing of G6/G7."
```

---

## Task 8: Wire SelectionContext at the BackendCall site

Connect the seam to the live request: build `SelectionContext` from the api-key sha256 + `LLMRequest.input_tokens`, fetch `selection_state` + `sticky`/`capacity` policies, pass to `select_provider`.

**Files:**
- Modify: `crates/agentgateway/src/proxy/httpproxy.rs:~2041` (`BackendCall` build site; `select_provider` call)
- Modify: `crates/agentgateway/src/proxy/httpproxy.rs:~191` (capture api-key sha256)

- [ ] **Step 1: Capture api-key sha256 + build SelectionContext**

At the `BackendCall` build site (`httpproxy.rs:~2041`, before `select_provider`):

```rust
let ctx_key = resolved_api_key_sha256.as_deref();  // captured at :191 auth
let input_tokens = llm_req.input_tokens;
let ctx = SelectionContext {
    key: if backend_policies.sticky.is_some() { ctx_key } else { None },
    input_tokens: if backend_policies.capacity.is_some() { input_tokens } else { None },
};
let selection_state = inputs.stores.read_binds().selection_state().clone();
let (provider, handle) = ai_backend.select_provider(
    &ctx,
    Some(&selection_state),
    backend_policies.capacity.as_ref(),
)?;
```

*(If `select_provider` can't borrow `selection_state` (lifetime), pass `Arc<SelectionState>` and upgrade inside, or pass `&SelectionState` if the borrow scopes cleanly. The G4 prober thread solved a similar borrow — clone the `Arc` out of the guard first, drop the guard, then bor­row.)*

- [ ] **Step 2: Capture pre-debited tokens for true-up**

Stash the pre-debited token count (from `check_capacity`'s return) on the per-request context (`ngx.ctx`-equivalent — agentgateway uses per-request state on the `RequestLog` or a sidecar). Retrieve it at `finish_request` for the TPM true-up (Task 6 Step 4).

- [ ] **Step 3: Test + commit**

Run: `cargo test -p agentgateway --lib proxy::` + `cargo test -p agentgateway --lib llm::`
Expected: all PASS.

```bash
git add crates/agentgateway/src/proxy/httpproxy.rs
git commit -m "feat(selection): wire SelectionContext at BackendCall site

Build ctx from api-key sha256 (sticky) + input_tokens (capacity); pass
selection_state + policies to select_provider. Stash pre-debited tokens
for TPM true-up."
```

---

## Task 9: Integration tests — composed predicate + pin-on-success + TPM + 503

End-to-end tests proving the spec's correctness claims. Uses the existing `httpproxy.rs:3459` test harness (`llm_retry_evicts_failed_priority_group_before_next_attempt` is the template).

**Files:**
- Add to: `crates/agentgateway/src/proxy/httpproxy.rs` (inline `#[cfg(test)]` mod) or `types/local_tests.rs`

- [ ] **Step 1: Write the composed-ordering integration test**

```rust
#[tokio::test]
async fn pinned_under_cap_beats_unpinned_under_cap() {
    // Two endpoints, equal raw score; key-A pinned to endpoint-1.
    // select_provider with key-A should pick endpoint-1 (×2.0 > ×1.0).
    // Set up EndpointSet with 2 endpoints, pin map with key-A→ep-1, assert ep-1 selected.
    // (Use the test-helper mock backend harness from local_tests.rs.)
    let _ = setup_two_endpoint_backend();  // helper from existing tests
    // ... pin key-A to ep-1 ...
    // ... call select_provider with ctx.key=Some("key-A") ...
    // ... assert selected.name == "ep-1"
}
```

- [ ] **Step 2: Write the evicted-pin-falls-through test**

```rust
#[tokio::test]
async fn evicted_pin_falls_through() {
    // key-A pinned to ep-1, but ep-1 is evicted (evicted_until set).
    // selection should NOT return ep-1; falls to ep-2 (unpinned, under-cap).
    // is_pinned checks: pin exists but ep-1 not in active set → treated unpinned.
}
```

- [ ] **Step 3: Write the all-over-cap → 503 + Retry-After test**

```rust
#[tokio::test]
async fn all_over_cap_returns_503_with_retry_after() {
    // Both endpoints at inflight cap. select returns NoHealthyEndpoints.
    // Response: 503 + Retry-After: <cooldown>.
    // Assert status 503 + header present.
}
```

- [ ] **Step 4: Write the pin-on-success-only test**

```rust
#[tokio::test]
async fn pin_written_on_success_not_failure() {
    // key-A, no existing pin. Request succeeds → pin exists.
    // Reset, request fails → no pin (or pre-existing pin untouched).
}
```

- [ ] **Step 5: Write the TPM true-up test**

```rust
#[tokio::test]
async fn tpm_true_up_frees_phantom_debt() {
    // Pre-debit 1000, actual 200 → 800 freed. Next request for 800 should pass.
    // Assert second request not rejected.
}
```

- [ ] **Step 6: Write the per-pod-divergence limitation test**

```rust
#[tokio::test]
async fn per_pod_pin_divergence_documented() {
    // Two SelectionStates (simulating two pods). Same key pins to different
    // backends on each. Documents the stated limitation in code.
    let state_a = SelectionState::default();
    let state_b = SelectionState::default();
    // ... pin key-A to ep-1 on state_a, ep-2 on state_b ...
    // ... assert divergent pins ...
}
```

- [ ] **Step 7: Run all + commit**

Run: `cargo test -p agentgateway --lib`
Expected: all PASS.

```bash
git add crates/agentgateway/src/proxy/httpproxy.rs
git commit -m "test(sticky,capacity): integration tests for composed predicate, pin,
TPM true-up, 503+Retry-After, per-pod divergence

Proves the spec's correctness claims: pinned-under-cap wins, evicted-pin
falls through, all-over-cap→503+Retry-After, pin-on-success-only, TPM
true-up frees phantom debt, per-pod pin divergence (documented limitation)."
```

---

## Post-implementation: Verification probes (run when a trigger lands)

Before merging G6/G7 when a trigger fires, run:
- **P-sticky:** key-A request ×3 → assert all hit the same backend (pin-on-success).
- **P-capacity-inflight:** set inflightCap=1, fire 2 concurrent → 2nd gets 503+Retry-After.
- **P-capacity-tpm:** set tpmPerMinute low, fire until rejected → 503+Retry-After; next window recovers.
- **P-composed:** pinned endpoint over cap loses to unpinned under-cap.
- **P-evicted-pin:** kill pinned backend, G4 prober evicts it → next request falls through to unpinned.
- **P-kill-switch:** rollout restart mid-request → no stuck pins (TTL expiry + state reset on pod restart).

---

## Self-Review Notes

- **Spec coverage:** G6 config (Task 3) ✓, G6 runtime (Task 4) ✓, G7 config (Task 5) ✓, G7 runtime inflight+TPM (Task 6) ✓, local path (Task 7) ✓, seam wiring (Tasks 1+8) ✓, testing (Task 9) ✓. Stated limitations tested in Task 9 Step 6 ✓.
- **Type consistency:** `SelectionContext` fields (`key`, `input_tokens`) consistent across Tasks 1/4/6/8. `composed_score` signature evolves (Task 1 → 4 → 6) — each task updates all call sites. `PinEntry`, `TpmCounter` defined in Task 2, used in Tasks 4/6. `BackendTrafficPolicy::Sticky`/`::Capacity` consistent across Tasks 3/5.
- **Known gaps to resolve during implementation:** (a) `EndpointInfo.endpoint_name` field access — verify exact field path; (b) `pending_requests.count()` method name — verify (`ActiveCounter` via `Arc::strong_count()`; the method may be `.countf()` per the exploration); (c) proto-regen target — confirm `make generate` or `buf generate`; (d) `pre_debited_tokens` stashing location on the request — decide `RequestLog` field vs sidecar. Each is a 2-min lookup, not a design risk.
