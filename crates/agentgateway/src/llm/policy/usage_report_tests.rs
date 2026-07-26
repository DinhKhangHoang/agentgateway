use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{InFlightLimiter, UsageReport, UsageReportPayload, send};
use crate::cel::LLMContext;
use crate::types::agent::{SimpleBackendReference, Target};

/// Build an LLMContext whose completion carries a sentinel we can grep for.
///
/// `LLMContext` has no `Default` impl (it is `#[apply(schema!)]`, which derives
/// Debug/Clone/Serialize/Deserialize only), so this is a full struct literal.
/// Do not add a `Default` impl to `LLMContext` just for this test.
fn ctx_with_sentinel() -> LLMContext {
	LLMContext {
		streaming: true,
		request_model: "gpt-4o".into(),
		response_model: Some("gpt-4o-2024-08-06".into()),
		provider: "openai".into(),
		input_tokens: Some(400004),
		input_image_tokens: None,
		input_text_tokens: None,
		input_audio_tokens: None,
		cached_input_tokens: None,
		cache_creation_input_tokens: None,
		output_tokens: Some(105),
		output_image_tokens: None,
		output_text_tokens: None,
		output_audio_tokens: None,
		reasoning_tokens: None,
		total_tokens: Some(400109),
		service_tier: None,
		first_token: None,
		time_to_first_token: Some(chrono::Duration::milliseconds(412).into()),
		time_per_output_token: Some(chrono::Duration::milliseconds(7).into()),
		count_tokens: None,
		prompt: None,
		completion: Some(vec!["SENTINEL_COMPLETION_TEXT".to_string()]),
		tool_calls: None,
		params: crate::llm::LLMRequestParams::default(),
		cost: None,
		cost_rates: None,
		cost_status: None,
	}
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
	// Durations render through the same formatter CelDuration's Serialize uses,
	// so a receiver sees the same string it would see in an access log.
	assert_eq!(v["timing"]["timeToFirstToken"], "0.412s");
	assert_eq!(v["timing"]["timePerOutputToken"], "0.007s");
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
	assert!(
		v["cost"]["input"].is_string(),
		"cost must serialize as a string"
	);
	assert_eq!(v["cost"]["input"], "1.20001");
	assert_eq!(v["cost"]["total"], "1.20106");
}

#[test]
fn traceparent_is_included_when_present_and_omitted_when_not() {
	let tp =
		::http::HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
	let with = UsageReportPayload::project(&ctx_with_sentinel(), Some(&tp), Default::default());
	let v: serde_json::Value = serde_json::to_value(&with).unwrap();
	assert_eq!(
		v["traceparent"],
		"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
	);

	let without = UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default());
	let v: serde_json::Value = serde_json::to_value(&without).unwrap();
	assert!(v.get("traceparent").is_none());
}

#[test]
fn dimensions_are_passed_through() {
	let dims = std::collections::BTreeMap::from([("tenant".to_string(), "user-11374".to_string())]);
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, dims);
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert_eq!(v["dimensions"]["tenant"], "user-11374");
}

// --- Delivery ---------------------------------------------------------------

/// A receiver that always answers with `status`, and counts what it received.
async fn receiver(status: u16) -> MockServer {
	let mock = MockServer::start().await;
	Mock::given(method("POST"))
		.and(path("/usage"))
		.respond_with(ResponseTemplate::new(status))
		.mount(&mock)
		.await;
	mock
}

fn report_to(mock: &MockServer, max_retries: u32) -> UsageReport {
	UsageReport {
		target: SimpleBackendReference::InlineBackend(Target::Address(*mock.address())),
		path: None,
		timeout: None,
		max_retries,
		dimensions: vec![],
	}
}

/// An unreachable target: `SimpleBackendReference::Invalid` fails to resolve in
/// `call_reference` before any connection is attempted.
fn unreachable_report() -> UsageReport {
	UsageReport {
		target: SimpleBackendReference::Invalid,
		path: None,
		timeout: None,
		max_retries: 0,
		dimensions: vec![],
	}
}

fn payload() -> UsageReportPayload {
	UsageReportPayload::project(&ctx_with_sentinel(), None, Default::default())
}

/// An unreachable receiver must not panic, must not block, and must be
/// counted. Losing a usage report silently is the failure mode this whole
/// counter exists to make visible.
#[tokio::test]
async fn unreachable_receiver_increments_dropped_counter() {
	let client = crate::test_helpers::policy_client();

	send(&unreachable_report(), payload(), client.clone()).await;

	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"an undeliverable usage report must be counted as dropped"
	);
}

/// max_retries controls attempts, not just the retry count: 0 retries means
/// exactly one attempt.
///
/// The plan specified asserting this against an `outbound_calls` metric
/// family. No such family exists — the only outbound-call metric is the
/// `upstream_call_duration` histogram, and it is recorded at
/// httpproxy.rs:4022, *after* `resolve_simple_backend` at :4001, so an
/// unresolvable target records nothing at all. Counting requests that actually
/// arrived at a receiver tests the same property and is not a proxy for it.
#[tokio::test]
async fn zero_retries_makes_exactly_one_attempt() {
	let mock = receiver(500).await;
	let client = crate::test_helpers::policy_client();

	send(&report_to(&mock, 0), payload(), client.clone()).await;

	assert_eq!(
		mock.received_requests().await.unwrap().len(),
		1,
		"max_retries=0 must mean exactly one attempt"
	);
	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"a report the receiver rejected is still a dropped report"
	);
}

/// A 5xx is retried up to max_retries times, then given up on.
#[tokio::test]
async fn rejected_report_is_retried_then_dropped() {
	let mock = receiver(500).await;
	let client = crate::test_helpers::policy_client();

	send(&report_to(&mock, 2), payload(), client.clone()).await;

	assert_eq!(
		mock.received_requests().await.unwrap().len(),
		3,
		"max_retries=2 must mean three attempts in total"
	);
	assert_eq!(client.inputs.metrics.llm_usage_report_dropped.get(), 1);
}

/// The happy path: one attempt, nothing dropped, and the body the receiver
/// gets is the payload we projected.
#[tokio::test]
async fn accepted_report_is_sent_once_and_not_dropped() {
	let mock = receiver(200).await;
	let client = crate::test_helpers::policy_client();

	send(&report_to(&mock, 2), payload(), client.clone()).await;

	let reqs = mock.received_requests().await.unwrap();
	assert_eq!(reqs.len(), 1, "a report accepted first try must not repeat");
	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		0,
		"a delivered report must not be counted as dropped"
	);

	let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
	assert_eq!(body["usage"]["inputTokens"], 400004);
	assert_eq!(
		reqs[0].headers.get("content-type").unwrap(),
		"application/json"
	);
}

/// The configured path must be used verbatim; the default is `/usage`.
#[tokio::test]
async fn configured_path_overrides_the_default() {
	let mock = MockServer::start().await;
	Mock::given(method("POST"))
		.and(path("/v1/billing/ingest"))
		.respond_with(ResponseTemplate::new(200))
		.mount(&mock)
		.await;
	let client = crate::test_helpers::policy_client();

	let mut cfg = report_to(&mock, 0);
	cfg.path = Some("/v1/billing/ingest".into());
	send(&cfg, payload(), client.clone()).await;

	assert_eq!(mock.received_requests().await.unwrap().len(), 1);
	assert_eq!(client.inputs.metrics.llm_usage_report_dropped.get(), 0);
}

// --- In-flight cap ----------------------------------------------------------

/// A wedged receiver must not let in-flight reports grow without bound. Past
/// the cap, reports are shed and counted rather than queued.
#[tokio::test]
async fn in_flight_reports_are_capped() {
	let limiter = InFlightLimiter::new(2);

	let a = limiter.try_acquire().expect("first permit");
	let _b = limiter.try_acquire().expect("second permit");
	assert!(
		limiter.try_acquire().is_none(),
		"third concurrent report must be shed, not queued"
	);

	drop(a);
	assert!(
		limiter.try_acquire().is_some(),
		"permit must be reusable once released"
	);
}

/// A dimension whose CEL expression fails must be omitted, and the report
/// must still be sent. Dropping a billing record because an operator typo'd
/// one label expression would be a wildly disproportionate failure.
#[test]
fn failing_dimension_expression_omits_only_that_dimension() {
	let ctx = ctx_with_sentinel();
	// Constructed the way llm/mod.rs does for the streaming amend path.
	let exec = crate::cel::Executor::new_llm_rate_limit_streaming(None, &ctx);

	let dims = vec![
		(
			"model".into(),
			std::sync::Arc::new(crate::cel::Expression::new_strict("llm.requestModel").unwrap()),
		),
		(
			// No request snapshot is set, so this cannot resolve.
			"tenant".into(),
			std::sync::Arc::new(
				crate::cel::Expression::new_strict(r#"request.headers["x-tenant"]"#).unwrap(),
			),
		),
	];

	let out = crate::llm::eval_dimensions(&dims, &exec);

	assert_eq!(
		out.get("model").map(String::as_str),
		Some("gpt-4o"),
		"the working dimension must survive its neighbour failing"
	);
	assert!(
		!out.contains_key("tenant"),
		"a dimension that cannot be evaluated must be omitted, not sent empty"
	);
}

/// Non-string dimension values must still land as usable labels.
#[test]
fn non_string_dimension_values_are_rendered() {
	let ctx = ctx_with_sentinel();
	let exec = crate::cel::Executor::new_llm_rate_limit_streaming(None, &ctx);
	let dims = vec![(
		"input".into(),
		std::sync::Arc::new(crate::cel::Expression::new_strict("llm.inputTokens").unwrap()),
	)];

	let out = crate::llm::eval_dimensions(&dims, &exec);

	assert_eq!(out.get("input").map(String::as_str), Some("400004"));
}
