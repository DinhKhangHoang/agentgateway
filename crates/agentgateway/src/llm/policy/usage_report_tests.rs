use super::UsageReportPayload;
use crate::cel::LLMContext;

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
	let dims =
		std::collections::BTreeMap::from([("tenant".to_string(), "user-11374".to_string())]);
	let payload = UsageReportPayload::project(&ctx_with_sentinel(), None, dims);
	let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
	assert_eq!(v["dimensions"]["tenant"], "user-11374");
}

/// A dimension whose CEL expression fails must be omitted, and the report
/// must still be sent. Dropping a billing record because an operator typo'd
/// one label expression would be a wildly disproportionate failure.
///
/// Ignored until `eval_dimensions` exists (Task 4, Step 5). Remove the
/// `#[ignore]` and fill in the body then — do not delete this test.
#[test]
#[ignore]
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
