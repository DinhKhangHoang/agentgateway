# Native LLM Usage Report — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let agentgateway POST final LLM token usage to a configured HTTP endpoint once per request, on completion, so external accounting systems no longer need an ext_proc service re-parsing every SSE chunk.

**Architecture:** A third arm on the existing on-completion fan-out. `AmendOnDrop::drop()` → `report_usage()` → `amend_tokens()` today feeds a local token bucket and an Envoy RLS server; this adds a configured HTTP sink alongside them. Nothing about request handling, streaming, or the response path changes.

**Tech Stack:** Rust (agentgateway proxy), protobuf/buf (xDS API), Go + controller-gen (CRD and controller translation).

**Design doc:** `agentgateway-llm-usage-report-design.md` — read it first. It records *why* the payload is a hand-written projection rather than a serde derive, and why there is no `failureMode`.

---

## File Structure

**New files:**

| File | Responsibility |
|---|---|
| `crates/agentgateway/src/llm/policy/usage_report.rs` | The `UsageReport` config type, the `UsageReportPayload` projection, and the send-with-retry logic. Mirrors `llm/policy/webhook.rs`. |
| `crates/agentgateway/src/llm/policy/usage_report_tests.rs` | Unit tests for the above. Follows the `#[cfg(test)] mod` convention used by `webhook.rs`/`tests.rs`. |

**Modified files:**

| File | Change |
|---|---|
| `crates/protos/proto/resource.proto:1168-1192` | New `UsageReport` message; new arm in `TrafficPolicySpec.oneof kind`. |
| `crates/agentgateway/src/types/agent_xds.rs` (near `:2465`) | Proto → Rust conversion. |
| `crates/agentgateway/src/types/agent.rs:2669` | New `TrafficPolicy::UsageReport` variant. |
| `crates/agentgateway/src/store/binds.rs:358` | `RoutePolicies.usage_report`. |
| `crates/agentgateway/src/store/binds.rs:~968` | Merge arm in the `TrafficPolicy` match. |
| `crates/agentgateway/src/store/binds.rs:458` | `LLMRequestPolicies.usage_report`. |
| `crates/agentgateway/src/store/binds.rs:534` | `LLMResponsePolicies.usage_report`. |
| `crates/agentgateway/src/proxy/httpproxy.rs:503` | Populate it. |
| `crates/agentgateway/src/llm/mod.rs:2539` | Third arm in `amend_tokens`. |
| `crates/agentgateway/src/llm/mod.rs:2586` | **Widen the gate.** See Task 5. |
| `crates/agentgateway/src/llm/policy/mod.rs:25` | `pub mod usage_report;` |
| `crates/agentgateway/src/telemetry/metrics.rs:165` | `OutboundCallSubtype::UsageReport` + dropped-report counter. |
| `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go:881` | `UsageReport` Go type + traffic field. |
| `controller/pkg/agentgateway/plugins/traffic_plugin.go:500` | `processUsageReportPolicy`. |

**Task order rationale:** Tasks 1–3 build and fully test the feature in isolation, with no wiring. Task 4 wires it into the proxy. Only then do Tasks 6–7 expose it through the xDS API and the CRD. This means every task after Task 1 has something real to test against, and a failure in the plumbing can never be mistaken for a failure in the logic.

**A note on code completeness:** Where the surrounding API was verified against the source, this plan gives literal code. Where the change is a mechanical repeat of an existing pattern, it names the exact file and line to mirror and states the substitutions — follow that code, do not invent a new shape. Signatures that could not be verified without reading further are called out explicitly with a read-first step; that is a real step, not a deferral.

---

## Task 1: Config type and payload projection

**Files:**
- Create: `crates/agentgateway/src/llm/policy/usage_report.rs`
- Create: `crates/agentgateway/src/llm/policy/usage_report_tests.rs`
- Modify: `crates/agentgateway/src/llm/policy/mod.rs:25`

This task is pure data. No network, no wiring. It is where the privacy property is established and locked down.

- [ ] **Step 1: Read the shapes this mirrors**

Read these three, in this order. Do not skip — the payload projection is only correct relative to what `LLMContext` actually contains:

- `crates/agentgateway/src/llm/policy/mod.rs:1527-1545` — the `Webhook` config struct, including the `#[apply(schema!)]` attribute and `serde_as` map handling. `UsageReport` copies this shape.
- `crates/agentgateway/src/cel/types.rs:1394-1500` — `LLMContext`. Note `prompt`, `completion` and `tool_calls` near `:1482`. These must **never** reach the payload.
- `crates/agentgateway/src/llm/cost/catalog.rs:206-226` — `Breakdown` and its `total()`.

- [ ] **Step 2: Write the failing tests**

Create `crates/agentgateway/src/llm/policy/usage_report_tests.rs`:

```rust
use super::usage_report::UsageReportPayload;
use crate::cel::types::LLMContext;

/// Build an LLMContext whose prompt and completion carry a sentinel we can grep for.
fn ctx_with_sentinel() -> LLMContext {
	let mut ctx = LLMContext::default();
	ctx.provider = strng::new("openai");
	ctx.request_model = strng::new("gpt-4o");
	ctx.response_model = Some(strng::new("gpt-4o-2024-08-06"));
	ctx.streaming = true;
	ctx.input_tokens = Some(400004);
	ctx.output_tokens = Some(105);
	ctx.total_tokens = Some(400109);
	ctx.completion = Some(vec!["SENTINEL_COMPLETION_TEXT".to_string()]);
	ctx
}

/// The payload must be an explicit projection. If someone replaces it with a
/// serde derive on LLMContext, every user prompt starts flowing to the
/// accounting endpoint. This test is the tripwire for that.
#[test]
fn payload_never_contains_conversation_content() {
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());
	let body = serde_json::to_string(&payload).unwrap();
	assert!(
		!body.contains("SENTINEL_COMPLETION_TEXT"),
		"completion text leaked into the usage report body: {body}"
	);
	assert!(
		!body.contains("prompt") && !body.contains("toolCalls"),
		"conversation fields leaked into the usage report body: {body}"
	);
}

#[test]
fn payload_carries_token_counts_and_model() {
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert_eq!(v["provider"], "openai");
	assert_eq!(v["requestModel"], "gpt-4o");
	assert_eq!(v["responseModel"], "gpt-4o-2024-08-06");
	assert_eq!(v["streaming"], true);
	assert_eq!(v["usage"]["inputTokens"], 400004);
	assert_eq!(v["usage"]["outputTokens"], 105);
	assert_eq!(v["usage"]["totalTokens"], 400109);
}

/// Unset optional counters are omitted, not sent as null, matching the
/// skip_serializing_if on LLMContext itself.
#[test]
fn payload_omits_unset_counters() {
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert!(v["usage"].get("inputAudioTokens").is_none());
	assert!(v["usage"].get("cacheCreationInputTokens").is_none());
}

/// Costs go into billing ledgers. Serializing Decimal as a JSON number invites
/// a float round-trip on the receiver; strings do not.
#[test]
fn cost_decimals_serialize_as_strings() {
	let mut ctx = ctx_with_sentinel();
	ctx.cost = Some(crate::llm::cost::Breakdown {
		input: "1.20001".parse().unwrap(),
		output: "0.00105".parse().unwrap(),
		..Default::default()
	});
	let payload = UsageReportPayload::project(&ctx, None, Default::default());
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert!(v["cost"]["input"].is_string(), "cost must serialize as a string");
	assert_eq!(v["cost"]["input"], "1.20001");
	assert_eq!(v["cost"]["total"], "1.20106");
}

#[test]
fn traceparent_is_included_when_present_and_omitted_when_not() {
	let tp = ::http::HeaderValue::from_static(
		"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
	);
	let with = UsageReportPayload::project(&ctx_with_sentinel(), Some(&tp), Default::default());
	let v: serde_json::Value = serde_json::to_value(&with).unwrap();
	assert_eq!(v["traceparent"], "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");

	let without = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());
	let v: serde_json::Value = serde_json::to_value(&without).unwrap();
	assert!(v.get("traceparent").is_none());
}

#[test]
fn dimensions_are_passed_through() {
	let dims = std::collections::BTreeMap::from([
		("tenant".to_string(), "user-11374".to_string()),
	]);
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, dims);
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert_eq!(v["dimensions"]["tenant"], "user-11374");
}

/// A dimension whose CEL expression fails must be omitted, and the report
/// must still be sent. Dropping a billing record because an operator typo'd
/// one label expression would be a wildly disproportionate failure.
#[test]
fn failing_dimension_expression_omits_only_that_dimension() {
	// Build two dimension expressions: one that evaluates, one that cannot
	// (e.g. referencing an absent header with a strict accessor). Evaluate
	// them through eval_dimensions and assert:
	//   - the working dimension is present with its value
	//   - the failing dimension is absent
	//   - eval_dimensions returned a map rather than an error
	// Construct the Executor the way llm/mod.rs:2592 does
	// (cel::Executor::new_llm_rate_limit_streaming).
}
```

The last test's body must be filled in once `eval_dimensions` exists (Task 4, Step 5). Write it here as `#[test] #[ignore]` if it cannot compile yet, and remove the `#[ignore]` in Task 4 — do not delete the test.

If `LLMContext` does not implement `Default`, construct it with the struct literal instead — read `cel/types.rs:1394` and fill every field. Do not add a `Default` impl to `LLMContext` just for the test.

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test --package agentgateway usage_report
```

Expected: compilation failure, `unresolved import ... usage_report`.

- [ ] **Step 4: Write `usage_report.rs`**

Create `crates/agentgateway/src/llm/policy/usage_report.rs` with the config type and the projection. The config type mirrors `Webhook` at `llm/policy/mod.rs:1527`:

```rust
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::cel::types::LLMContext;
use crate::types::agent::SimpleBackendReference;
use crate::*;

/// Default path on the receiving endpoint.
pub const DEFAULT_PATH: &str = "/usage";
/// Default per-attempt timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
/// Default number of retries after a failed attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

#[apply(schema!)]
pub struct UsageReport {
	/// Backend that receives usage reports.
	pub target: SimpleBackendReference,
	/// Request path. Defaults to `/usage`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub path: Option<Strng>,
	/// Per-attempt timeout. Defaults to 2s.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timeout: Option<Duration>,
	/// Retries after a failed delivery attempt. Defaults to 2.
	#[serde(default, skip_serializing_if = "crate::serdes::is_default")]
	pub max_retries: u32,
	/// Extra report dimensions, computed from CEL expressions evaluated
	/// against the original incoming request.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[serde_as(as = "serde_with::Map<_, _>")]
	pub dimensions: Vec<(Strng, Arc<cel::Expression>)>,
}

/// Token counters. Every field is optional and omitted when unset, mirroring
/// LLMContext. Field names match the CEL `llm.*` attribute names so that a
/// receiver reading access logs and a receiver reading usage reports see the
/// same vocabulary.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCounters {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub input_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub input_text_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub input_image_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub input_audio_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cached_input_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cache_creation_input_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output_text_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output_image_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output_audio_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reasoning_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub count_tokens: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub total_tokens: Option<u64>,
}

/// USD cost components. Serialized as strings so a receiver cannot lose
/// precision by parsing a billing figure as a float.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
	pub input: String,
	pub cache_read: String,
	pub cache_write: String,
	pub output: String,
	pub reasoning: String,
	pub input_audio: String,
	pub output_audio: String,
	pub total: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTiming {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub time_to_first_token: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub time_per_output_token: Option<String>,
}

/// The wire payload.
///
/// This is deliberately a hand-written projection of `LLMContext` and must stay
/// that way. `LLMContext` also carries `prompt`, `completion` and `tool_calls`
/// (cel/types.rs:1482-1492); deriving Serialize on it, or adding a catch-all
/// field here, would ship every user prompt and model completion to the
/// accounting endpoint. `payload_never_contains_conversation_content` guards
/// this.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportPayload {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub traceparent: Option<String>,
	pub provider: String,
	pub request_model: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub response_model: Option<String>,
	pub streaming: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub service_tier: Option<String>,
	pub usage: UsageCounters,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cost: Option<UsageCost>,
	pub timing: UsageTiming,
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub dimensions: BTreeMap<String, String>,
}

impl UsageReportPayload {
	pub fn project(
		ctx: &LLMContext,
		traceparent: Option<&::http::HeaderValue>,
		dimensions: BTreeMap<String, String>,
	) -> Self {
		Self {
			traceparent: traceparent
				.and_then(|v| v.to_str().ok())
				.map(str::to_string),
			provider: ctx.provider.to_string(),
			request_model: ctx.request_model.to_string(),
			response_model: ctx.response_model.as_ref().map(ToString::to_string),
			streaming: ctx.streaming,
			service_tier: ctx.service_tier.as_ref().map(ToString::to_string),
			usage: UsageCounters {
				input_tokens: ctx.input_tokens,
				input_text_tokens: ctx.input_text_tokens,
				input_image_tokens: ctx.input_image_tokens,
				input_audio_tokens: ctx.input_audio_tokens,
				cached_input_tokens: ctx.cached_input_tokens,
				cache_creation_input_tokens: ctx.cache_creation_input_tokens,
				output_tokens: ctx.output_tokens,
				output_text_tokens: ctx.output_text_tokens,
				output_image_tokens: ctx.output_image_tokens,
				output_audio_tokens: ctx.output_audio_tokens,
				reasoning_tokens: ctx.reasoning_tokens,
				count_tokens: ctx.count_tokens,
				total_tokens: ctx.total_tokens,
			},
			cost: ctx.cost.as_ref().map(|c| UsageCost {
				input: c.input.to_string(),
				cache_read: c.cache_read.to_string(),
				cache_write: c.cache_write.to_string(),
				output: c.output.to_string(),
				reasoning: c.reasoning.to_string(),
				input_audio: c.input_audio.to_string(),
				output_audio: c.output_audio.to_string(),
				total: c.total().to_string(),
			}),
			timing: UsageTiming {
				time_to_first_token: ctx.time_to_first_token.as_ref().map(|d| format!("{d:?}")),
				time_per_output_token: ctx.time_per_output_token.as_ref().map(|d| format!("{d:?}")),
			},
			dimensions,
		}
	}
}

#[cfg(test)]
#[path = "usage_report_tests.rs"]
mod tests;
```

Register the module in `crates/agentgateway/src/llm/policy/mod.rs`, next to `pub mod webhook;` at line 25:

```rust
pub mod usage_report;
```

Adjust `CelDuration` formatting in `timing` if `{:?}` does not produce the intended form — check what `CelDuration` renders and prefer a stable string like `0.412s`. Add an assertion for the exact format to `payload_carries_token_counts_and_model` once you have confirmed it.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --package agentgateway usage_report
```

Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/agentgateway/src/llm/policy/usage_report.rs \
        crates/agentgateway/src/llm/policy/usage_report_tests.rs \
        crates/agentgateway/src/llm/policy/mod.rs
git commit -m "feat(llm): add usage report config type and payload projection"
```

---

## Task 2: Delivery — send with retry, bounded concurrency, metrics

**Files:**
- Modify: `crates/agentgateway/src/telemetry/metrics.rs:165` and the metrics registry
- Modify: `crates/agentgateway/src/llm/policy/usage_report.rs`
- Modify: `crates/agentgateway/src/llm/policy/usage_report_tests.rs`

- [ ] **Step 1: Read the send pattern and the metrics registry**

- `crates/agentgateway/src/llm/policy/webhook.rs:209-231` — `send_request`. This is the exact `PolicyClient` call shape: `with_outbound(kind, subtype).call_reference(req, &target)`.
- `crates/agentgateway/src/llm/policy/mod.rs:18-23` — `with_default_timeout`, which inserts `BackendRequestTimeout` into the request extensions. The configured timeout is applied the same way.
- `crates/agentgateway/src/telemetry/metrics.rs:152-185` — `OutboundCallKind`, `OutboundCallSubtype`, `OutboundCallLabels`.
- `crates/agentgateway/src/llm/policy/tests.rs:1-56` — the test harness: `crate::test_helpers::policy_client()`, `SimpleBackendReference::Invalid` to force an unreachable target, and metric assertion via `client.inputs.metrics.<family>.get_or_create(&labels).get()`.

Find where counter families are declared in the `Metrics` struct in `metrics.rs` and follow that pattern for the new counter.

- [ ] **Step 2: Add the metrics**

In `crates/agentgateway/src/telemetry/metrics.rs:165`, add the subtype:

```rust
pub enum OutboundCallSubtype {
	// Primary
	#[default]
	Http,
	Llm,
	Mcp,

	// Policy
	ExtAuthz,
	ExtProc,
	Guardrail,
	RateLimit,
	Oidc,
	UsageReport,
}
```

Add a counter to the `Metrics` struct, following the declaration style of the families already there:

```rust
/// Usage reports abandoned after the final retry, or shed because the
/// in-flight cap was reached. Non-zero means unbilled usage: alert on it.
pub llm_usage_report_dropped: counter::Counter,
```

Register it with the name `agentgateway_llm_usage_report_dropped_total` wherever the other families are registered in the same file.

- [ ] **Step 3: Write the failing delivery tests**

Append to `crates/agentgateway/src/llm/policy/usage_report_tests.rs`:

```rust
use crate::types::agent::SimpleBackendReference;

fn unreachable_report() -> super::usage_report::UsageReport {
	super::usage_report::UsageReport {
		target: SimpleBackendReference::Invalid,
		path: None,
		timeout: None,
		max_retries: 0,
		dimensions: vec![],
	}
}

/// An unreachable receiver must not panic, must not block, and must be
/// counted. Losing a usage report silently is the failure mode this whole
/// counter exists to make visible.
#[tokio::test]
async fn unreachable_receiver_increments_dropped_counter() {
	let client = crate::test_helpers::policy_client();
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());

	super::usage_report::send(&unreachable_report(), payload, client.clone())
		.await;

	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"an undeliverable usage report must be counted as dropped"
	);
}

/// max_retries controls attempts, not just the retry count: 0 retries means
/// exactly one attempt.
#[tokio::test]
async fn zero_retries_makes_exactly_one_attempt() {
	let client = crate::test_helpers::policy_client();
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());

	super::usage_report::send(&unreachable_report(), payload, client.clone())
		.await;

	let attempts = client
		.inputs
		.metrics
		.outbound_calls
		.get_or_create(&crate::telemetry::metrics::OutboundCallLabels {
			kind: crate::telemetry::metrics::OutboundCallKind::Policy,
			subtype: crate::telemetry::metrics::OutboundCallSubtype::UsageReport,
		})
		.get();
	assert_eq!(attempts, 1, "max_retries=0 must mean exactly one attempt");
}
```

Use whatever the outbound-call family is actually named in `metrics.rs` — read it in Step 1 and substitute. Write `send` as an `async fn` so tests can await it directly; the spawning happens at the call site in Task 4, not inside `send`. That separation is what makes this testable at all.

- [ ] **Step 4: Run the tests to verify they fail**

```bash
cargo test --package agentgateway usage_report
```

Expected: `cannot find function 'send'`.

- [ ] **Step 5: Implement `send`**

Add to `usage_report.rs`, modelled on `webhook.rs:209-231`:

```rust
/// Deliver one usage report, retrying on failure.
///
/// Never returns an error: the response has already reached the client, so
/// there is nothing a caller could do with one. Failures are recorded on
/// `llm_usage_report_dropped` instead, which is the metric operators alert on.
pub async fn send(cfg: &UsageReport, payload: UsageReportPayload, client: PolicyClient) {
	let attempts = cfg.max_retries.saturating_add(1);
	let mut backoff = Duration::from_millis(50);

	for attempt in 0..attempts {
		match try_send_once(cfg, &payload, &client).await {
			Ok(status) if status.is_success() => return,
			Ok(status) => {
				debug!("usage report attempt {attempt} rejected with status {status}");
			},
			Err(e) => {
				debug!("usage report attempt {attempt} failed: {e}");
			},
		}
		if attempt + 1 < attempts {
			tokio::time::sleep(backoff).await;
			backoff *= 2;
		}
	}

	client.inputs.metrics.llm_usage_report_dropped.inc();
	warn!(
		provider = %payload.provider,
		model = %payload.request_model,
		"usage report dropped after {attempts} attempts; usage for this request is unbilled"
	);
}

async fn try_send_once(
	cfg: &UsageReport,
	payload: &UsageReportPayload,
	client: &PolicyClient,
) -> anyhow::Result<::http::StatusCode> {
	let body = serde_json::to_vec(payload)?;
	let path = cfg.path.as_deref().unwrap_or(DEFAULT_PATH);
	let mut req = ::http::Request::builder()
		.method(::http::Method::POST)
		.uri(path)
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(crate::http::Body::from(body))?;
	req.extensions_mut().insert(BackendRequestTimeout(
		cfg.timeout.unwrap_or(DEFAULT_TIMEOUT),
	));

	let res = client
		.with_outbound(OutboundCallKind::Policy, OutboundCallSubtype::UsageReport)
		.call_reference(req, &cfg.target)
		.await?;
	Ok(res.status())
}
```

The exact `Request`/`Body` construction must match what `webhook.rs` does — read `build_request_for_request` in `webhook.rs` and follow it rather than the sketch above if they differ.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test --package agentgateway usage_report
```

Expected: 8 passed.

- [ ] **Step 7: Commit**

```bash
git add crates/agentgateway/src/llm/policy/usage_report.rs \
        crates/agentgateway/src/llm/policy/usage_report_tests.rs \
        crates/agentgateway/src/telemetry/metrics.rs
git commit -m "feat(llm): deliver usage reports with retry and drop accounting"
```

---

## Task 3: Bounded in-flight concurrency

**Files:**
- Modify: `crates/agentgateway/src/llm/policy/usage_report.rs`
- Modify: `crates/agentgateway/src/llm/policy/usage_report_tests.rs`

The existing amend spawns an unbounded task per request (`remoteratelimit.rs:145`). Under a wedged receiver that is an unbounded task pile-up. This task adds the cap before the call site exists, so it can never be forgotten.

- [ ] **Step 1: Write the failing test**

```rust
/// A wedged receiver must not let in-flight reports grow without bound. Past
/// the cap, reports are shed and counted rather than queued.
#[tokio::test]
async fn in_flight_reports_are_capped() {
	let client = crate::test_helpers::policy_client();
	let limiter = super::usage_report::InFlightLimiter::new(2);

	let _a = limiter.try_acquire().expect("first permit");
	let _b = limiter.try_acquire().expect("second permit");
	assert!(
		limiter.try_acquire().is_none(),
		"third concurrent report must be shed, not queued"
	);

	drop(_a);
	assert!(limiter.try_acquire().is_some(), "permit must be reusable once released");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --package agentgateway in_flight_reports_are_capped
```

Expected: `cannot find type 'InFlightLimiter'`.

- [ ] **Step 3: Implement**

```rust
/// Caps concurrent in-flight usage reports so a slow or wedged receiver
/// cannot pile up unbounded tasks on the gateway.
#[derive(Clone)]
pub struct InFlightLimiter(Arc<tokio::sync::Semaphore>);

/// Maximum concurrent in-flight usage reports per gateway.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 1024;

impl InFlightLimiter {
	pub fn new(max: usize) -> Self {
		Self(Arc::new(tokio::sync::Semaphore::new(max)))
	}
	/// Returns None when the cap is reached. Callers must shed and count,
	/// never await a permit: blocking here would apply backpressure to
	/// request completion, which is exactly what this must not do.
	pub fn try_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
		self.0.clone().try_acquire_owned().ok()
	}
}

impl Default for InFlightLimiter {
	fn default() -> Self {
		Self::new(DEFAULT_MAX_IN_FLIGHT)
	}
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --package agentgateway usage_report
```

Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/agentgateway/src/llm/policy/usage_report.rs \
        crates/agentgateway/src/llm/policy/usage_report_tests.rs
git commit -m "feat(llm): cap in-flight usage reports"
```

---

## Task 4: Wire into the response path — and fix the gate

**Files:**
- Modify: `crates/agentgateway/src/store/binds.rs:534` (`LLMResponsePolicies`)
- Modify: `crates/agentgateway/src/store/binds.rs:458` (`LLMRequestPolicies`)
- Modify: `crates/agentgateway/src/proxy/httpproxy.rs:503`
- Modify: `crates/agentgateway/src/llm/mod.rs:2539` and `:2586`
- Modify: `crates/agentgateway/src/llm/tests.rs:71`

**This is the task the design doc warns about.** `report_usage()` is gated on rate limiting being configured. Miss the gate and the feature silently no-ops for exactly the deployments that need it.

- [ ] **Step 1: Write the failing gate regression test**

In `crates/agentgateway/src/llm/tests.rs`, next to the existing `LLMResponsePolicies` construction at `:71`:

```rust
use crate::llm::policy::usage_report::UsageReport;
use crate::types::agent::SimpleBackendReference;

/// Local to this module. Task 2 defines a same-named helper in
/// usage_report_tests.rs; that one is not in scope here, so this is a
/// deliberate duplicate rather than an import.
fn unreachable_report() -> UsageReport {
	UsageReport {
		target: SimpleBackendReference::Invalid,
		path: None,
		timeout: None,
		max_retries: 0,
		dimensions: vec![],
	}
}

/// A usage report must fire when it is the ONLY policy configured.
///
/// report_usage() is gated on rate limiting being present (llm/mod.rs:2586).
/// Deployments whose limits live in an external policy server configure no
/// native rate limit at all, so a gate that only checks rate limits makes the
/// feature silently do nothing for precisely its intended users. This test
/// fails if that gate regresses.
#[tokio::test]
async fn usage_report_fires_with_no_rate_limit_configured() {
	let client = crate::test_helpers::policy_client();
	let pol = LLMResponsePolicies {
		local_rate_limit: vec![],
		remote_rate_limit: None,
		request_traceparent: None,
		prompt_guard: vec![],
		streaming_prompt_guard_enabled: false,
		usage_report: Some(Arc::new(unreachable_report())),
	};

	let mut amend = AmendOnDrop::new(/* ...as the existing tests construct it... */);
	amend.report_usage();

	// Delivery fails (target is Invalid); what matters is that it was ATTEMPTED.
	// A gate regression shows up as this counter staying at zero.
	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"usage report must be attempted even with no rate limit configured"
	);
}
```

Construct `AmendOnDrop` exactly as the surrounding tests in `llm/tests.rs` do — read them first. If `report_usage` spawns, await a short yield or make the spawn injectable so the test is deterministic; do **not** paper over it with a sleep.

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --package agentgateway usage_report_fires_with_no_rate_limit
```

Expected: compile error on the unknown `usage_report` field.

- [ ] **Step 3: Add the fields**

`store/binds.rs:534`:

```rust
pub struct LLMResponsePolicies {
	pub local_rate_limit: Vec<http::localratelimit::RateLimit>,
	pub remote_rate_limit: Option<http::remoteratelimit::LLMResponseAmend>,
	pub request_traceparent: Option<HeaderValue>,
	pub prompt_guard: Vec<ResponseGuard>,
	pub streaming_prompt_guard_enabled: bool,
	pub usage_report: Option<Arc<llm::policy::usage_report::UsageReport>>,
}
```

`store/binds.rs:458`:

```rust
pub struct LLMRequestPolicies {
	pub local_rate_limit: Option<Arc<Vec<http::localratelimit::RateLimit>>>,
	pub remote_rate_limit: Option<Arc<http::remoteratelimit::RemoteRateLimit>>,
	pub llm: Option<Arc<llm::Policy>>,
	pub usage_report: Option<Arc<llm::policy::usage_report::UsageReport>>,
}
```

- [ ] **Step 4: Populate it**

`proxy/httpproxy.rs:503`:

```rust
	Ok(store::LLMResponsePolicies {
		local_rate_limit,
		remote_rate_limit: response,
		request_traceparent: req.headers().get(TRACEPARENT).cloned(),
		prompt_guard: prompt_guard.map(|g| g.response.clone()).unwrap_or_default(),
		streaming_prompt_guard_enabled: prompt_guard.is_some_and(|g| g.streaming.is_enabled()),
		usage_report: policies.usage_report.clone(),
	})
```

- [ ] **Step 5: Widen the gate and add the third arm**

`llm/mod.rs:2586` — the gate:

```rust
	pub fn report_usage(&mut self) {
		if let Some(pol) = self.pol.take()
			&& (!pol.local_rate_limit.is_empty()
				|| pol.remote_rate_limit.is_some()
				|| pol.usage_report.is_some())
		{
```

`llm/mod.rs:2539` — the fan-out. Note `rate_limit.remote_rate_limit` is moved by the existing `if let`; a partial move is fine, the remaining fields stay accessible:

```rust
fn amend_tokens(rate_limit: store::LLMResponsePolicies, llm_resp: &LLMInfo, exec: Executor) {
	// ... existing input_mismatch / tokens_to_remove computation, unchanged ...

	for lrl in &rate_limit.local_rate_limit {
		lrl.amend_tokens(tokens_to_remove)
	}
	if let Some(ur) = &rate_limit.usage_report {
		let dimensions = eval_dimensions(&ur.dimensions, &exec);
		let ctx = LLMContext::from_llm_info(llm_resp.clone(), None);
		let payload = usage_report::UsageReportPayload::project(
			&ctx,
			rate_limit.request_traceparent.as_ref(),
			dimensions,
		);
		let (ur, client) = (ur.clone(), /* PolicyClient, see Step 6 */);
		if let Some(permit) = in_flight.try_acquire() {
			tokio::task::spawn(async move {
				let _permit = permit;
				usage_report::send(&ur, payload, client).await;
			});
		} else {
			client.inputs.metrics.llm_usage_report_dropped.inc();
		}
	}
	if let Some(rrl) = rate_limit.remote_rate_limit {
		rrl.amend_tokens(tokens_to_remove, &exec)
	}
}
```

`eval_dimensions` evaluates each CEL expression against `exec` and skips any that fail — a broken dimension expression must omit that dimension, never drop the report. Model it on how `remoteratelimit.rs:161-167` handles a failing cost expression.

- [ ] **Step 6: Thread the `PolicyClient` and the limiter**

`amend_tokens` has neither today. `AmendOnDrop` (`llm/mod.rs:2562`) is the natural carrier: add `client: PolicyClient` and `in_flight: InFlightLimiter` fields, set in `AmendOnDrop::new`, and pass them through `report_usage`. Update `AmendOnDrop::new`'s callers accordingly — find them with:

```bash
rg 'AmendOnDrop::new' crates/
```

- [ ] **Step 7: Fix the existing test constructor**

`crates/agentgateway/src/llm/tests.rs:71` constructs `LLMResponsePolicies` literally and will no longer compile. Add `usage_report: None`.

- [ ] **Step 8: Run the full suite**

```bash
cargo test --all-targets
```

Expected: all pass, including `usage_report_fires_with_no_rate_limit_configured`.

- [ ] **Step 9: Commit**

```bash
git add crates/agentgateway/src/
git commit -m "feat(llm): fire usage reports on request completion

Widens the report_usage gate to include usage_report. Without this the
feature silently no-ops for deployments whose limits live in an external
policy server rather than in a native rate limit policy."
```

---

## Task 5: Streaming and non-streaming integration tests

**Files:**
- Modify: `crates/agentgateway/src/llm/tests.rs`

Tasks 1–4 tested units. This proves the end-to-end behaviour that motivated the feature: **one report per request, not one per chunk.**

- [ ] **Step 1: Read the existing streaming test setup and pick a fixture**

Find an existing multi-chunk SSE test to borrow from:

```bash
rg -n 'stream' crates/agentgateway/src/llm/tests.rs | head -20
rg -n 'stream' crates/agentgateway/src/llm/anthropic_tests.rs | head -20
```

Record two things before writing any test: how the fixture drives a response through the LLM response path, and how many SSE chunks it emits. The chunk count matters — a fixture with one chunk cannot distinguish one-report-per-request from one-report-per-chunk, and would make this whole task vacuous. If every available fixture is single-chunk, extend one to at least three chunks first.

- [ ] **Step 2: Build a counting receiver**

A test double that accepts POSTs, counts them, and retains the last body:

```rust
#[derive(Clone, Default)]
struct CountingReceiver {
	count: Arc<std::sync::atomic::AtomicUsize>,
	last: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

impl CountingReceiver {
	fn request_count(&self) -> usize {
		self.count.load(std::sync::atomic::Ordering::SeqCst)
	}
	fn last_payload(&self) -> serde_json::Value {
		self.last.lock().unwrap().clone().expect("no report received")
	}
}
```

Serve it however the surrounding tests serve in-process backends, and point the `UsageReport.target` at it. If no such helper exists, the simplest workable form is a `tokio::net::TcpListener` on port 0 plus a `SimpleBackendReference` to its address.

- [ ] **Step 3: Write the failing tests**

```rust
/// The entire point of this feature: a streaming response must produce
/// exactly ONE usage report, not one per SSE chunk. If this asserts a number
/// greater than 1, the implementation is doing what the ext_proc service it
/// replaces was doing, and has bought nothing.
#[tokio::test]
async fn streaming_response_produces_exactly_one_report() {
	let receiver = CountingReceiver::default();
	// ...drive the multi-chunk SSE fixture from Step 1 through the response
	// path, with a usage_report policy targeting `receiver`...

	assert_eq!(receiver.request_count(), 1, "one report per request, not per chunk");
	let body = receiver.last_payload();
	assert_eq!(body["streaming"], true);
	assert_eq!(body["usage"]["outputTokens"], /* the fixture's total */);
}

#[tokio::test]
async fn non_streaming_response_produces_exactly_one_report() {
	let receiver = CountingReceiver::default();
	// ...drive the non-streaming fixture...

	assert_eq!(receiver.request_count(), 1);
	let body = receiver.last_payload();
	assert_eq!(body["streaming"], false);
}
```

The driving lines are left to be written against the fixture API read in Step 1. The assertions are fixed: do not weaken `request_count() == 1` to a range, and do not drop the `outputTokens` check — a report that arrives once but carries zero tokens is just as broken as ten reports.

- [ ] **Step 4: Run, implement any gaps, run again**

```bash
cargo test --package agentgateway --lib llm::tests
```

Expected: both pass, both asserting exactly 1.

- [ ] **Step 5: Commit**

```bash
git add crates/agentgateway/src/llm/tests.rs
git commit -m "test(llm): one usage report per request, streaming and not"
```

---

## Task 6: xDS API — proto and conversion

**Files:**
- Modify: `crates/protos/proto/resource.proto:1168-1192`
- Modify: `crates/agentgateway/src/types/agent.rs:2669`
- Modify: `crates/agentgateway/src/types/agent_xds.rs` (near `:2465`)
- Modify: `crates/agentgateway/src/store/binds.rs:358` and `:~968`

- [ ] **Step 1: Add the proto message and oneof arm**

In `crates/protos/proto/resource.proto`, inside `TrafficPolicySpec` before the `oneof kind` block:

```protobuf
  message UsageReport {
    BackendReference target = 1;
    optional string path = 2;
    google.protobuf.Duration timeout = 3;
    uint32 max_retries = 4;
    message Dimension {
      string key = 1;
      string value = 2; // CEL expr
    }
    repeated Dimension dimensions = 5;
  }
```

Add to the `oneof kind` at `:1168`, after `Delay delay = 23;`:

```protobuf
    UsageReport usage_report = 24;
```

- [ ] **Step 2: Regenerate**

```bash
make generate-apis
```

Expected: `crates/protos/src/` regenerated with the new message. Verify with:

```bash
rg 'UsageReport' crates/protos/src/ | head
```

- [ ] **Step 3: Add the Rust policy variant**

`crates/agentgateway/src/types/agent.rs:2669`, in `enum TrafficPolicy` after `ExtProc`:

```rust
	UsageReport(RequestPolicy<llm::policy::usage_report::UsageReport>),
```

- [ ] **Step 4: Add the xDS conversion**

`crates/agentgateway/src/types/agent_xds.rs`. Mirror the `RemoteRateLimit` arm at `:2465` — it is the closest analogue because it also resolves a `BackendReference` target and compiles CEL expression strings. Substitutions: `RemoteRateLimit` → `UsageReport`, descriptor entries → dimensions, and the `type`/`cost`/`limit_override` fields have no counterpart here.

```rust
		Some(tps::Kind::UsageReport(ur)) => {
			// ... mirror :2465, resolving `target` and compiling each
			// dimension value as a CEL expression ...
		},
```

- [ ] **Step 5: Add the route policy plumbing**

`store/binds.rs:358`, in `RoutePolicies` after `ext_proc`:

```rust
	pub usage_report: RequestPolicy<llm::policy::usage_report::UsageReport>,
```

`store/binds.rs:~968`, in the `TrafficPolicy` match beside the `ExtProc` arm:

```rust
				TrafficPolicy::UsageReport(p) => {
					pol.usage_report.merge_with_inheritance(p, lock_inheritance);
				},
```

Then populate `LLMRequestPolicies.usage_report` from `RoutePolicies.usage_report` wherever `local_rate_limit` and `remote_rate_limit` are transferred.

- [ ] **Step 6: Build and test**

```bash
cargo test --all-targets
```

Expected: pass. A non-exhaustive-match error means an arm was missed — fix it, do not add a catch-all.

- [ ] **Step 7: Commit**

```bash
git add crates/protos/ crates/agentgateway/src/types/ crates/agentgateway/src/store/
git commit -m "feat(xds): plumb usageReport traffic policy"
```

---

## Task 7: CRD and controller translation

**Files:**
- Modify: `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go:881`
- Modify: `controller/pkg/agentgateway/plugins/traffic_plugin.go:500`

- [ ] **Step 1: Add the Go type**

In `agentgateway_policy_types.go`, near the other traffic policy types. Use the type definition verbatim from the design doc §1 — it is complete, including the doc comments that record the non-durability and the deliberate absence of `failureMode`.

Add the field to the traffic policy struct beside `ExtProc` at `:881`:

```go
	// Reports final LLM token usage to an external endpoint on request completion.
	// +optional
	UsageReport *UsageReport `json:"usageReport,omitempty"`
```

- [ ] **Step 2: Add the controller translation**

`controller/pkg/agentgateway/plugins/traffic_plugin.go`. Mirror `processExtProcPolicy` at `:1365-1435` — it resolves a `BackendObjectReference` to an `api.BackendReference` and builds the `api.TrafficPolicySpec_*` wrapper, which is exactly the shape needed.

```go
// processUsageReportPolicy converts a UsageReport CRD policy into the
// corresponding agentgateway traffic policy.
func processUsageReportPolicy(
	ctx PolicyCtx,
	usageReport *agentgateway.UsageReport,
	// ...remaining params as processExtProcPolicy takes them...
) (*api.Policy, error) {
```

Register it at `:500` beside the `ExtProc` dispatch:

```go
	if traffic.UsageReport != nil {
		// ...mirror the ExtProc block at :501-503...
	}
```

Also add `UsageReport` to the policy-enumeration at `:2229` where `s.Traffic.ExtProc` is walked, so status reporting and reference resolution see it.

- [ ] **Step 3: Regenerate CRDs and deepcopy**

```bash
make gen
```

Expected: `zz_generated.deepcopy.go` gains `UsageReport` methods, and the CRD YAML gains the `usageReport` property. Verify:

```bash
rg -l 'usageReport' controller/ | head
```

- [ ] **Step 4: Add a translation testdata case**

Follow the existing pattern in `controller/pkg/agentgateway/plugins/testdata/` — add an input policy with `usageReport` and its expected translated output.

- [ ] **Step 5: Run the controller tests**

```bash
go test ./controller/...
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add controller/
git commit -m "feat(controller): translate usageReport policy"
```

---

## Task 8: Docs, example, and final checks

- [ ] **Step 1: Add an example config**

Follow the pattern in `examples/` — a `config.json` exercising `usageReport`. Check whether `make test` validates every `examples/*/config.json` (the `objects := $(wildcard examples/*/config.json)` line in the Makefile suggests it does) and make sure the new one passes.

- [ ] **Step 2: Regenerate the JSON schema**

```bash
make generate-schema
```

- [ ] **Step 3: Full lint and test**

```bash
make lint
cargo test --all-targets
go test ./controller/...
```

Expected: clean. `make lint` runs `cargo fmt --check` with non-default import settings, so run `make format` first if it complains.

- [ ] **Step 4: Verify the repo is clean after codegen**

```bash
make check-clean-repo
```

Expected: no diff. A diff here means generated output was committed inconsistently.

- [ ] **Step 5: Commit**

```bash
git add .
git commit -m "docs(llm): usage report example and generated schema"
```

---

## Out of scope

**The pilot migration is a separate plan.** It depends on this feature existing and on a policy-server change, and it lives in the `agw-pilot` worktree, not here. In outline: the policy server grows `POST /v1/usage` settling by traceparent; ext_proc keeps the request path for guardrails and sets `responseBodyMode: None`; then `pilot/verify/phase5/sse_timing.py` runs as a controlled same-session A/B, with acceptance being that TTFT, chunk count and inter-chunk gaps return to the pre-ext_proc baseline.

Also out of scope, per the design doc's open questions: batching, durable delivery, and gRPC transport.
