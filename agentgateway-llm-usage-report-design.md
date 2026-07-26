# Native LLM Usage Report — Design

**Status:** design, approved for planning
**Date:** 2026-07-26
**Repo:** agentgateway (this repo)
**Motivating consumer:** the MaaS-v2 pilot in `user-11377-maas-v2-agw`

---

## Goal

Let agentgateway report final LLM token usage to a configured HTTP endpoint once,
when a request completes, so that external accounting systems no longer need an
ext_proc service sitting on the response path parsing every SSE chunk.

## Why

The MaaS-v2 pilot runs an ext_proc service whose sole response-path job is to
re-parse SSE chunks to recover the `usage` object and true up Redis token
counters. In `FullDuplexStreamed` mode every response chunk round-trips
gateway → processor → gateway (`http/ext_proc/buffering.rs:342-355`), which is a
measurable cost on a live SSE stream.

That parse is redundant. agentgateway **already** parses every SSE chunk for its
own provider translation and metrics — `amend_from_stream_response`
(`crates/llm/src/types/detect.rs:515`) is what populates
`gen_ai_server_time_to_first_token`. The token counts the pilot wants are already
in memory at the end of the request.

## What already exists

This is the important framing: **the log phase is already built.** It is not a
gap to fill, it is a fan-out to extend.

```
AmendOnDrop::drop()          llm/mod.rs:2613
  └─ report_usage()          llm/mod.rs:2586
       └─ amend_tokens()     llm/mod.rs:2539
            ├─ local token bucket        localratelimit.rs:225
            └─ Envoy RLS (hits_addend)   remoteratelimit.rs:138
```

`amend_tokens` computes `input_mismatch + output_tokens` and pushes it to each
sink. `report_usage` is called explicitly at end-of-stream
(`crates/llm/src/types/detect.rs:606`) with the `Drop` impl as a safety net.

**This design adds a third arm to that fan-out.** Nothing else moves.

## Why not just use the existing RLS sink

Because the pilot charges an up-front estimate and settles afterwards, so
settlement can be negative (a refund). `hits_addend` is `UInt64Value`
(`crates/protos/proto/rls.proto:241`) — unsigned. Negative amendments are
unrepresentable in the Envoy RLS protocol, which is why
`remoteratelimit.rs:169` silently skips them:

```rust
// We cannot currently do negative amendments, so if its negative just skip
```

Extending RLS with a signed field would break every stock RLS server and is not
upstreamable. A separate sink with its own contract is the honest answer.

> Note for the record: agentgateway's own model — `hits_addend = 0` at request
> time (`remoteratelimit.rs:237`), full charge on completion — has no refund
> concept at all and needs none. It was considered and rejected for the pilot in
> favour of keeping the tighter admission bound that an up-front reservation
> gives under burst.

---

## Design

### 1. Configuration surface

`usageReport` is **traffic-scoped**, not backend-scoped.

`spec.backend.ai` is only valid on a Backend of type `ai`
(`controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go:351-357`);
with one Backend per model that would mean configuring accounting ~140 times for
one tenant policy. `LLMResponsePolicies` already mixes scopes — `prompt_guard`
is backend-scoped, but `local_rate_limit` and `remote_rate_limit` come from
traffic-scoped policies (`proxy/httpproxy.rs:477-497`). Usage reporting is an
accounting sink, so it follows the rate-limit precedent.

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayPolicy
metadata:
  name: maas-usage-report
  namespace: user-11377-maas-v2-agw
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  traffic:
    usageReport:
      backendRef:
        name: maas-policy-server
        port: 8080
      # Optional. Defaults to /usage.
      path: /v1/usage
      # Optional. Defaults to 2s.
      timeout: 2s
      # Optional. Bounded retry on delivery failure. Defaults to 2.
      maxRetries: 2
      # CEL expressions evaluated against the original request, like the
      # guardrail webhook's `headers` field.
      dimensions:
        tenant: 'request.headers["x-maas-tenant"]'
        keyHash: 'request.headers["x-maas-key-sha"]'
```

**There is deliberately no `failureMode`.** Every other callout in the tree has
one and copying it here would be cargo-culting. This callout fires after the
response has already reached the client; there is no request left to reject, so
"fail closed" has no meaning. Delivery failure handling is §4 instead.

Go types (`controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go`),
alongside the other traffic policies:

```go
// UsageReport reports final LLM token usage to an external endpoint once per
// request, after the response completes. Intended for accounting and billing
// systems that need actual token counts rather than estimates.
//
// The report is delivered asynchronously and is not durable: reports in flight
// when the gateway exits are lost. Consumers needing an audit trail should
// reconcile against access logs, which carry the same token counts.
type UsageReport struct {
	// Endpoint that receives usage reports.
	// Supported types: `Service` and `Backend`.
	// +required
	BackendRef gwv1.BackendObjectReference `json:"backendRef"`

	// Request path on the endpoint. Defaults to `/usage`.
	// +optional
	Path *string `json:"path,omitempty"`

	// Per-attempt timeout. Defaults to 2s.
	// +optional
	Timeout *metav1.Duration `json:"timeout,omitempty"`

	// Number of retries after a failed delivery attempt. Defaults to 2
	// (three attempts total). Set to 0 to disable retries.
	// +optional
	// +kubebuilder:validation:Minimum=0
	// +kubebuilder:validation:Maximum=5
	MaxRetries *int32 `json:"maxRetries,omitempty"`

	// Additional dimensions to include in the report, as CEL expressions
	// evaluated against the original incoming request.
	// +optional
	// +kubebuilder:validation:MaxProperties=32
	Dimensions map[string]CELExpression `json:"dimensions,omitempty"`
}
```

Field is added to the traffic policy struct at
`agentgateway_policy_types.go:881`-ish, next to `ExtProc` and `RateLimit`:

```go
	// Reports final LLM token usage to an external endpoint on request completion.
	// +optional
	UsageReport *UsageReport `json:"usageReport,omitempty"`
```

### 2. Rust config type

Mirrors `llm::policy::Webhook` (`llm/policy/mod.rs:1527`), which is the closest
existing shape:

```rust
#[apply(schema!)]
pub struct UsageReport {
	/// Backend that receives usage reports.
	pub target: SimpleBackendReference,
	/// Request path. Defaults to `/usage`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub path: Option<Strng>,
	/// Per-attempt timeout.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timeout: Option<Duration>,
	/// Retries after a failed attempt.
	#[serde(default, skip_serializing_if = "crate::serdes::is_default")]
	pub max_retries: u32,
	/// Extra report dimensions, computed from CEL expressions against the
	/// original incoming request.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[serde_as(as = "serde_with::Map<_, _>")]
	pub dimensions: Vec<(Strng, Arc<cel::Expression>)>,
}
```

Carried on `LLMResponsePolicies` (`store/binds.rs:534`):

```rust
pub struct LLMResponsePolicies {
	pub local_rate_limit: Vec<http::localratelimit::RateLimit>,
	pub remote_rate_limit: Option<http::remoteratelimit::LLMResponseAmend>,
	pub request_traceparent: Option<HeaderValue>,
	pub prompt_guard: Vec<ResponseGuard>,
	pub streaming_prompt_guard_enabled: bool,
	pub usage_report: Option<Arc<UsageReport>>,   // new
}
```

Populated at `proxy/httpproxy.rs:503` from the traffic policy set, the same way
`local_rate_limit` is.

### 3. Payload contract

**Absolute actuals, never a delta.**

Sending actuals is what dissolves the signedness problem rather than working
around it. The receiver already knows what it reserved, so it computes the
settlement itself — refunds become arithmetic on the receiver's side and never
touch the wire. It also makes the report naturally idempotent under retry, which
a delta can never be.

The idempotency key already exists. `LLMResponsePolicies.request_traceparent`
(`store/binds.rs:537`) is the traceparent captured at request time, plainly added
so the asynchronous amend can be correlated back. extAuth reserves under the
traceparent; the report settles under it. No new plumbing.

`LLMContext::from_llm_info` is already constructed at `llm/mod.rs:2591`, one line
above the amend call, with every field needed (`cel/types.rs:1394`).

**The payload must be an explicit projection of `LLMContext`, never a blanket
`serde` serialization of it.** `LLMContext` also carries `prompt`, `completion`
and `tool_calls` (`cel/types.rs:1482-1492`) — the actual conversation content.
Serializing the struct wholesale would ship every user prompt and model
completion to the accounting endpoint, which is a data-exfiltration bug wearing
a convenience costume. A dedicated `UsageReportPayload` struct with explicitly
listed fields is the only acceptable form, and a test should assert that no
prompt or completion text appears in the serialized body.

`POST /v1/usage`
`Content-Type: application/json`

```json
{
  "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
  "provider": "openai",
  "requestModel": "gpt-4o",
  "responseModel": "gpt-4o-2024-08-06",
  "streaming": true,
  "serviceTier": "default",
  "usage": {
    "inputTokens": 400004,
    "cachedInputTokens": 0,
    "outputTokens": 105,
    "reasoningTokens": 0,
    "totalTokens": 400109
  },
  "cost": {
    "input": "1.20001", "cacheRead": "0", "cacheWrite": "0",
    "output": "0.00105", "reasoning": "0",
    "inputAudio": "0", "outputAudio": "0",
    "total": "1.20106"
  },
  "timing": {
    "timeToFirstToken": "0.412s",
    "timePerOutputToken": "0.019s"
  },
  "dimensions": {
    "tenant": "user-11374",
    "keyHash": "9f2b..."
  }
}
```

`usage` carries the full set of `Option<u64>` counters from `LLMContext` —
including the multi-modal `inputText`/`inputImage`/`inputAudio` and
`outputText`/`outputImage`/`outputAudio` splits, `cacheCreationInputTokens`, and
`countTokens` — omitted rather than sent as null, matching the existing
`skip_serializing_if = "Option::is_none"` on the struct. The example above shows
only the fields a plain text chat completion populates.

`cost` mirrors `llm::cost::Breakdown` (`llm/cost/catalog.rs:206-214`): seven
per-component USD `Decimal`s plus the derived `total()`. It is absent entirely
when the model could not be priced. Decimals are serialized as **strings**, not
JSON numbers, so receivers cannot silently lose precision through a float parse
— this matters when the value is going into a billing ledger.

**Response:** any 2xx means delivered. The body is ignored. Non-2xx and transport
errors are retried per §4. The gateway deliberately reads nothing back — a
usage report cannot influence a response that has already been sent, and giving
the receiver a channel to do so would be a trap.

### 4. Delivery semantics

Three deliberate departures from the existing amend path, which is
fire-and-forget (`remoteratelimit.rs:145-147`):

```rust
tokio::task::spawn(async move {
    let _ = self.base.check_internal(self.client, self.request).await;
});
```

1. **Bounded retry with backoff.** A dropped rate-limit update is a rounding
   error; a dropped accounting record is unbilled revenue. Default 2 retries,
   exponential backoff, per-attempt timeout from config.

2. **Delivery observability.** Add a variant to `OutboundCallSubtype`
   (`telemetry/metrics.rs:165`):

   ```rust
   pub enum OutboundCallSubtype {
       Http, Llm, Mcp,
       ExtAuthz, ExtProc, Guardrail, RateLimit, Oidc,
       UsageReport,   // new
   }
   ```

   Delivery rate, latency and status then come free from the existing
   outbound-call metrics, via the same
   `client.with_outbound(OutboundCallKind::Policy, OutboundCallSubtype::UsageReport)`
   pattern webhook.rs uses. Plus one dedicated counter,
   `agentgateway_llm_usage_report_dropped_total`, for reports abandoned after
   the final retry — this is the number an operator alerts on.

3. **Bounded in-flight concurrency.** The existing code spawns an unbounded task
   per request. Under a slow or wedged receiver that is an unbounded task
   pile-up on the gateway. Cap concurrent in-flight reports with a semaphore;
   when the cap is reached, drop and increment the dropped counter rather than
   queueing without limit.

**Not durable, by design.** Reports in flight when the pod exits are lost. This
is documented on the Go type rather than hidden. Access-log OTLP already carries
the same token counts (`telemetry/log.rs:776-806`) and is the intended
reconciliation backstop for anyone who needs an audit trail.

### 5. The gate that will silently break this

`report_usage()` is guarded at `llm/mod.rs:2586-2589`:

```rust
if let Some(pol) = self.pol.take()
    && (!pol.local_rate_limit.is_empty() || pol.remote_rate_limit.is_some())
```

If that condition is not widened to include `usage_report.is_some()`, the
feature does nothing at all for any deployment not *also* using native rate
limiting — which is exactly the MaaS-v2 configuration, since its limits live in
the external policy server. It would pass a unit test that configures a rate
limit alongside the report, and fail silently in production.

This is called out here because it is the single most likely way to ship a
broken version of this feature.

### 6. Call site

`amend_tokens` (`llm/mod.rs:2539`) gains a third arm. It is currently
synchronous and infallible; the report is spawned, so the signature does not
change:

```rust
fn amend_tokens(rate_limit: store::LLMResponsePolicies, llm_resp: &LLMInfo, exec: Executor) {
    // ... existing input_mismatch / tokens_to_remove computation ...

    for lrl in &rate_limit.local_rate_limit {
        lrl.amend_tokens(tokens_to_remove)
    }
    if let Some(rrl) = rate_limit.remote_rate_limit {
        rrl.amend_tokens(tokens_to_remove, &exec)
    }
    if let Some(ur) = &rate_limit.usage_report {
        usage_report::send(ur, llm_resp, rate_limit.request_traceparent.as_ref(), &exec);
    }
}
```

New module `crates/agentgateway/src/llm/policy/usage_report.rs`, modelled on
`webhook.rs:209-251`.

---

## Testing

Rust tests alongside `llm/policy/tests.rs`:

1. **Non-streaming request produces exactly one report** with the actual token
   counts from the response body.
2. **Streaming request produces exactly one report** at end-of-stream, not one
   per chunk. Asserts the receiver saw a single POST.
3. **Report fires with no rate-limit policy configured.** This is the §5 gate
   regression test and must exist before the gate is touched.
4. **Dimensions are evaluated from the original request**, including a failing
   CEL expression, which should omit the dimension rather than drop the report.
5. **Receiver returns 500 → retried, then dropped**, dropped counter
   increments, and the client response is unaffected.
6. **Receiver is unreachable → no panic, no leaked task**, counter increments.
7. **Traceparent is present in the report** when the incoming request carried
   one, and the report is still sent when it did not.
8. **No conversation content leaks.** Drive a request whose prompt and
   completion contain a distinctive sentinel string, then assert the sentinel
   appears nowhere in the serialized report body. This guards the §3 projection
   against someone later "simplifying" it into a `serde` derive on
   `LLMContext`, which would silently start exfiltrating prompts.
9. **Cost decimals serialize as strings**, not JSON numbers, so a receiver
   cannot lose precision parsing them as floats.

## Pilot migration (separate, follows this feature)

extAuth is untouched and keeps reserving the estimate. Then:

1. Policy server grows `POST /v1/usage`, settling against the reservation by
   traceparent. The settlement arithmetic — including refunds — is the same code
   the ext_proc service uses today, just behind a different entry point.
2. ext_proc keeps the request path for per-tenant guardrails (Phase 6 gap 1) and
   sets `responseBodyMode: None`
   (`agentgateway_policy_types.go:2508` confirms `None` is a valid
   `BodySendMode`), leaving the response path entirely.
3. Re-run `pilot/verify/phase5/sse_timing.py` as a controlled same-session A/B.
   Acceptance: TTFT, chunk count and inter-chunk gaps return to the
   pre-ext_proc baseline. This is the whole point of the exercise and is the
   measurement that decides whether it worked.
4. `MAX_REQUEST_BODY_BYTES` on the ext_proc service stays as-is — it governs the
   request path, which is unchanged. See `pilot/18-policy-buffer.yaml`.

## Open questions

- **Retry defaults.** 2 retries with exponential backoff is a guess. It should
  be revisited against the policy server's observed p99 once there is real
  traffic.
- **Batching.** Not in this design. At MaaS-v2's request rate a POST per request
  is fine, and batching would complicate the idempotency story. Worth revisiting
  only if the receiver becomes a bottleneck.
- **Dimensions as free-form CEL vs a fixed schema.** Free-form is chosen for
  consistency with `extProc.requestAttributes` and the guardrail webhook's
  `headers`. The cost is that the receiver contract is only as stable as the
  operator's config.
