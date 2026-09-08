use std::fs;
use std::path::{Path, PathBuf};

use agent_core::strng;
use http_body_util::BodyExt;
use serde_json::{Value, json};

use super::*;
use crate::http::x_headers::TRACEPARENT;

fn llm_request_with_tokens(input_tokens: Option<u64>) -> LLMRequest {
	LLMRequest {
		input_tokens,
		input_format: InputFormat::Completions,
		cache_convention: CacheTokenConvention::pending(),
		request_model: "test-model".into(),
		provider: "test-provider".into(),
		streaming: true,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	}
}

#[test]
fn vertex_gemini_uses_native_completions_and_compat_fallbacks() {
	let provider = AIProvider::Vertex(vertex::Provider {
		project_id: strng::new("test-project"),
		model: None,
		region: None,
	});
	let model = Some("google/gemini-2.5-flash-lite");

	assert_eq!(
		provider
			.chat_translation(InputFormat::Completions, model)
			.unwrap()
			.output,
		ChatFormat::VertexGemini
	);
	for input in [InputFormat::Messages, InputFormat::Responses] {
		assert_eq!(
			provider.chat_translation(input, model).unwrap().output,
			ChatFormat::OpenAICompletions
		);
	}
}

#[test]
fn streaming_amend_on_drop_updates_local_rate_limit() {
	let rate_limit =
		crate::http::localratelimit::RateLimit::try_from(crate::http::localratelimit::RateLimitSpec {
			max_tokens: 10,
			tokens_per_fill: 10,
			fill_interval: std::time::Duration::from_secs(60),
			limit_type: crate::http::localratelimit::RateLimitType::Tokens,
		})
		.unwrap();
	let log = AsyncLog::default();
	log.store(Some(LLMInfo {
		request: llm_request_with_tokens(Some(2)),
		response: LLMResponse {
			input_tokens: Some(2),
			output_tokens: Some(4),
			..Default::default()
		},
	}));

	let mut amend = AmendOnDrop::new(
		log,
		LLMResponsePolicies {
			local_rate_limit: vec![rate_limit.clone()],
			..Default::default()
		},
		None,
		None,
		crate::test_helpers::policy_client(),
	);
	let _ = amend.report_usage();

	assert!(
		rate_limit
			.check_llm_request(&llm_request_with_tokens(Some(7)))
			.is_err()
	);
	assert!(
		rate_limit
			.check_llm_request(&llm_request_with_tokens(Some(6)))
			.is_ok()
	);
}

/// Local to this module. `usage_report_tests.rs` defines a same-named helper;
/// that one is not in scope here, so this is a deliberate duplicate rather
/// than an import.
fn unreachable_report() -> crate::llm::policy::usage_report::UsageReport {
	crate::llm::policy::usage_report::UsageReport {
		target: crate::types::agent::SimpleBackendReference::Invalid,
		path: None,
		timeout: None,
		max_retries: Some(0),
		dimensions: vec![],
	}
}

/// A usage report must fire when it is the ONLY policy configured.
///
/// `report_usage()` is gated on rate limiting being present (llm/mod.rs:2588).
/// Deployments whose limits live in an external policy server configure no
/// native rate limit at all, so a gate that only checks rate limits makes the
/// feature silently do nothing for precisely its intended users. This test
/// fails if that gate regresses.
#[tokio::test]
async fn usage_report_fires_with_no_rate_limit_configured() {
	let client = crate::test_helpers::policy_client();
	let log = AsyncLog::default();
	log.store(Some(LLMInfo {
		request: llm_request_with_tokens(Some(2)),
		response: LLMResponse {
			input_tokens: Some(2),
			output_tokens: Some(4),
			..Default::default()
		},
	}));

	let mut amend = AmendOnDrop::new(
		log,
		LLMResponsePolicies {
			usage_report: Some(Arc::new(unreachable_report())),
			..Default::default()
		},
		None,
		None,
		client.clone(),
	);
	// report_usage hands back the delivery task so a test can await it. No
	// sleep, no yield: the assertion runs after delivery has actually finished.
	let delivery = amend.report_usage().expect("a report must be dispatched");
	delivery.await.unwrap();

	// Delivery fails (the target is Invalid); what matters is that it was
	// ATTEMPTED. A gate regression shows up as this counter staying at zero.
	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"usage report must be attempted even with no rate limit configured"
	);
}

/// Both completion paths — buffered and streaming — gate on
/// `needs_completion_amend`. This asserts the condition itself, because a
/// test that calls `amend_tokens` directly bypasses the gate entirely and so
/// cannot detect a gate regression: verified by reverting the gate and
/// watching such a test still pass.
#[test]
fn a_usage_report_alone_requires_a_completion_amend() {
	let pol = LLMResponsePolicies {
		usage_report: Some(Arc::new(unreachable_report())),
		..Default::default()
	};
	assert!(
		pol.needs_completion_amend(),
		"a usage report with no rate limit configured must still amend on completion"
	);
	assert!(
		!LLMResponsePolicies::default().needs_completion_amend(),
		"nothing configured must remain a no-op"
	);
}

/// The buffered path reaches delivery once the gate lets it through.
#[tokio::test]
async fn usage_report_fires_on_the_non_streaming_path_with_no_rate_limit() {
	let client = crate::test_helpers::policy_client();
	let pol = LLMResponsePolicies {
		usage_report: Some(Arc::new(unreachable_report())),
		..Default::default()
	};
	let llm_info = LLMInfo {
		request: llm_request_with_tokens(Some(2)),
		response: LLMResponse {
			input_tokens: Some(2),
			output_tokens: Some(4),
			..Default::default()
		},
	};
	let resp = ::http::Response::new(crate::http::Body::empty());
	let exec = cel::Executor::new_response(None, &resp);

	let delivery =
		amend_tokens(pol, &llm_info, exec, client.clone()).expect("a report must be dispatched");
	delivery.await.unwrap();

	assert_eq!(
		client.inputs.metrics.llm_usage_report_dropped.get(),
		1,
		"the buffered-response path must report usage too"
	);
}

// --- One report per request -------------------------------------------------

/// Accepts POSTs, counts them, keeps the last body, and signals arrivals so a
/// test can await delivery instead of sleeping.
#[derive(Clone)]
struct CountingReceiver {
	count: Arc<std::sync::atomic::AtomicUsize>,
	last: Arc<std::sync::Mutex<Option<Value>>>,
	tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl CountingReceiver {
	fn request_count(&self) -> usize {
		self.count.load(std::sync::atomic::Ordering::SeqCst)
	}
	fn last_payload(&self) -> Value {
		self
			.last
			.lock()
			.unwrap()
			.clone()
			.expect("no report received")
	}
}

impl wiremock::Respond for CountingReceiver {
	fn respond(&self, req: &wiremock::Request) -> wiremock::ResponseTemplate {
		self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		*self.last.lock().unwrap() = serde_json::from_slice(&req.body).ok();
		let _ = self.tx.send(());
		wiremock::ResponseTemplate::new(200)
	}
}

async fn counting_receiver() -> (
	wiremock::MockServer,
	CountingReceiver,
	tokio::sync::mpsc::UnboundedReceiver<()>,
) {
	let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
	let receiver = CountingReceiver {
		count: Default::default(),
		last: Default::default(),
		tx,
	};
	let mock = wiremock::MockServer::start().await;
	wiremock::Mock::given(wiremock::matchers::method("POST"))
		.and(wiremock::matchers::path("/usage"))
		.respond_with(receiver.clone())
		.mount(&mock)
		.await;
	(mock, receiver, rx)
}

fn report_policy_targeting(mock: &wiremock::MockServer) -> LLMResponsePolicies {
	LLMResponsePolicies {
		usage_report: Some(Arc::new(crate::llm::policy::usage_report::UsageReport {
			target: crate::types::agent::SimpleBackendReference::InlineBackend(
				crate::types::agent::Target::Address(*mock.address()),
			),
			path: None,
			timeout: None,
			max_retries: Some(0),
			dimensions: vec![],
		})),
		..Default::default()
	}
}

/// Await the first report. A bounded wait on a signal, not a fixed sleep: it
/// returns the moment delivery happens and fails loudly if it never does.
async fn await_first_report(rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
	tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
		.await
		.expect("no usage report arrived within 10s")
		.expect("receiver channel closed before a report arrived");
}

/// The entire point of this feature: a streaming response must produce exactly
/// ONE usage report, not one per SSE chunk. If this asserts a number greater
/// than 1, the implementation is doing what the ext_proc service it replaces
/// was doing, and has bought nothing.
///
/// The bedrock `basic.bin` fixture carries three contentBlockDelta events, so
/// a per-chunk implementation would show at least three reports here. A
/// single-chunk fixture would make this assertion vacuous.
#[tokio::test]
async fn streaming_response_produces_exactly_one_report() {
	use crate::test_helpers::proxymock::setup_proxy_test;

	let (mock, receiver, mut rx) = counting_receiver().await;
	let client = PolicyClient::new(setup_proxy_test("{}").unwrap().pi);

	let provider = AIProvider::bedrock(bedrock::Provider {
		model: Some(strng::new("us.anthropic.claude-haiku-4-5-20251001-v1:0")),
		region: strng::new("us-west-2"),
		guardrail_identifier: None,
		guardrail_version: None,
	});
	let req = LLMRequest {
		input_tokens: None,
		input_format: InputFormat::Messages,
		cache_convention: CacheTokenConvention::pending(),
		request_model: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
		provider: "bedrock".into(),
		streaming: true,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	};

	let input_bytes = fs::read(fixture_path("response/bedrock/basic.bin")).expect("fixture");
	let resp = Response::new(Body::from(input_bytes));

	let out = provider
		.process_response(
			client,
			req,
			report_policy_targeting(&mock),
			None,
			AsyncLog::default(),
			llm::LogContentFields::default(),
			None,
			resp,
		)
		.await
		.expect("streaming response should process");

	// Draining the body runs the stream to completion, which is what triggers
	// the single end-of-request report.
	let _ = out.collect().await.unwrap();
	await_first_report(&mut rx).await;

	assert_eq!(
		receiver.request_count(),
		1,
		"one report per request, not per chunk"
	);
	let body = receiver.last_payload();
	assert_eq!(body["streaming"], true);
	assert_eq!(body["usage"]["outputTokens"], 142);
	assert_eq!(body["usage"]["inputTokens"], 15);
}

#[tokio::test]
async fn non_streaming_response_produces_exactly_one_report() {
	use crate::test_helpers::proxymock::setup_proxy_test;

	let (mock, receiver, mut rx) = counting_receiver().await;
	let client = PolicyClient::new(setup_proxy_test("{}").unwrap().pi);

	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let req = LLMRequest {
		input_tokens: None,
		input_format: InputFormat::Completions,
		cache_convention: CacheTokenConvention::pending(),
		request_model: "gpt-3.5-turbo".into(),
		provider: "openai".into(),
		streaming: false,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	};

	let input_bytes = fs::read(fixture_path("response/completions/basic.json")).expect("fixture");
	let mut resp = Response::new(Body::from(input_bytes));
	resp.headers_mut().insert(
		::http::header::CONTENT_TYPE,
		"application/json".parse().unwrap(),
	);

	let out = provider
		.process_response(
			client,
			req,
			report_policy_targeting(&mock),
			None,
			AsyncLog::default(),
			llm::LogContentFields::default(),
			None,
			resp,
		)
		.await
		.expect("buffered response should process");
	let _ = out.collect().await.unwrap();

	await_first_report(&mut rx).await;

	assert_eq!(receiver.request_count(), 1);
	let body = receiver.last_payload();
	assert_eq!(body["streaming"], false);
	assert_eq!(body["usage"]["outputTokens"], 23);
	assert_eq!(body["usage"]["inputTokens"], 17);
}

fn test_root() -> &'static Path {
	Path::new("../llm/src/tests")
}

fn fixture_path(relative_path: &str) -> PathBuf {
	test_root().join(relative_path)
}

#[test]
fn response_prompt_guard_headers_copies_request_traceparent() {
	let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
		.parse()
		.unwrap();
	let mut response_headers = ::http::HeaderMap::new();
	response_headers.insert("x-upstream", "value".parse().unwrap());

	let headers = response_prompt_guard_headers(&response_headers, Some(&traceparent));

	assert_eq!(headers.get("x-upstream").unwrap(), "value");
	assert_eq!(
		headers.get(TRACEPARENT).unwrap(),
		"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
	);
	assert!(!response_headers.contains_key(TRACEPARENT));
}

#[test]
fn response_prompt_guard_headers_overwrites_upstream_traceparent() {
	let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
		.parse()
		.unwrap();
	let mut response_headers = ::http::HeaderMap::new();
	response_headers.insert(
		TRACEPARENT,
		"00-11111111111111111111111111111111-2222222222222222-01"
			.parse()
			.unwrap(),
	);

	let headers = response_prompt_guard_headers(&response_headers, Some(&traceparent));

	assert_eq!(
		headers.get(TRACEPARENT).unwrap(),
		"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
	);
	assert_eq!(
		response_headers.get(TRACEPARENT).unwrap(),
		"00-11111111111111111111111111111111-2222222222222222-01"
	);
}

#[tokio::test]
async fn test_passthrough() {
	let input_path = fixture_path("requests/completions/full.json");
	let openai_str = &fs::read_to_string(&input_path).expect("Failed to read input file");
	let openai_raw: Value = serde_json::from_str(openai_str).expect("Failed to parse input json");
	let openai: types::completions::Request =
		serde_json::from_str(openai_str).expect("Failed to parse input JSON");
	let t = serde_json::to_string_pretty(&openai).unwrap();
	let t2 = serde_json::to_string_pretty(&openai_raw).unwrap();
	assert_eq!(
		serde_json::from_str::<Value>(&t).unwrap(),
		serde_json::from_str::<Value>(&t2).unwrap(),
		"{t}\n{t2}"
	);
}

#[tokio::test]
async fn openai_provider_normalizes_max_tokens_before_forwarding() {
	use crate::http::auth::BackendInfo;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.openai.com", 443)),
		inputs,
	};
	let req = ::http::Request::builder()
		.uri("/v1/chat/completions")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "gpt-5.4",
				"max_tokens": 1024,
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_completions_request(&backend_info, None, req, false, &mut None)
		.await
		.expect("OpenAI completions request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert!(forwarded_json.get("max_tokens").is_none());
	assert_eq!(forwarded_json["max_completion_tokens"], json!(1024));
	assert_eq!(llm_request.params.max_tokens, Some(1024));
}

#[tokio::test]
async fn openai_provider_normalizes_max_tokens_after_model_alias() {
	use crate::http::auth::BackendInfo;
	use crate::llm::policy::Policy;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.openai.com", 443)),
		inputs,
	};
	let policy = Policy {
		model_aliases: std::collections::HashMap::from([(
			strng::new("fast-model"),
			strng::new("gpt-5.4"),
		)]),
		..Default::default()
	};
	let req = ::http::Request::builder()
		.uri("/v1/chat/completions")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "fast-model",
				"max_tokens": 1024,
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_completions_request(&backend_info, Some(&policy), req, false, &mut None)
		.await
		.expect("OpenAI completions request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(forwarded_json["model"], json!("gpt-5.4"));
	assert!(forwarded_json.get("max_tokens").is_none());
	assert_eq!(forwarded_json["max_completion_tokens"], json!(1024));
	assert_eq!(llm_request.request_model, "gpt-5.4");
	assert_eq!(llm_request.params.max_tokens, Some(1024));
}

#[tokio::test]
async fn openai_provider_preserves_max_tokens_for_non_gpt_models() {
	use crate::http::auth::BackendInfo;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("localhost", 11434)),
		inputs,
	};
	let req = ::http::Request::builder()
		.uri("/v1/chat/completions")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "llama3.1",
				"max_tokens": 1024,
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_completions_request(&backend_info, None, req, false, &mut None)
		.await
		.expect("OpenAI-compatible completions request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(forwarded_json["max_tokens"], json!(1024));
	assert!(forwarded_json.get("max_completion_tokens").is_none());
	assert_eq!(llm_request.params.max_tokens, Some(1024));
}

#[tokio::test]
async fn count_tokens_resolves_model_alias_once_for_upstream_request() {
	use crate::http::auth::BackendInfo;
	use crate::llm::policy::Policy;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::Anthropic(anthropic::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.anthropic.com", 443)),
		inputs,
	};
	let policy = Policy {
		model_aliases: std::collections::HashMap::from([
			(strng::new("short-name"), strng::new("middle-name")),
			(strng::new("middle-name"), strng::new("final-name")),
		]),
		..Default::default()
	};
	let req = ::http::Request::builder()
		.uri("/v1/messages/count_tokens")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "short-name",
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_count_tokens_request(&backend_info, req, Some(&policy), &mut None)
		.await
		.expect("count_tokens request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(forwarded_json["model"], json!("middle-name"));
	assert_eq!(llm_request.request_model, "middle-name");
}

#[tokio::test]
async fn count_tokens_uses_native_endpoint_after_model_alias() {
	use crate::http::auth::BackendInfo;
	use crate::llm::policy::Policy;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::Vertex(vertex::Provider {
		model: None,
		region: None,
		project_id: strng::new("test-project"),
	});
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("us-central1-aiplatform.googleapis.com", 443)),
		inputs,
	};
	let policy = Policy {
		model_aliases: std::collections::HashMap::from([(
			strng::new("short-name"),
			strng::new("claude-3-5-sonnet"),
		)]),
		..Default::default()
	};
	let req = ::http::Request::builder()
		.uri("/v1/messages/count_tokens")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "short-name",
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		upstream_route_type,
		..
	} = provider
		.process_count_tokens_request(&backend_info, req, Some(&policy), &mut None)
		.await
		.expect("count_tokens request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(upstream_route_type, RouteType::AnthropicTokenCount);
	assert_eq!(forwarded_json["model"], json!("claude-3-5-sonnet"));
	assert_eq!(llm_request.request_model, "claude-3-5-sonnet");
}

#[tokio::test]
async fn vertex_anthropic_messages_prepares_vertex_body() {
	use crate::http::auth::BackendInfo;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::Vertex(vertex::Provider {
		model: None,
		region: Some(strng::new("us-central1")),
		project_id: strng::new("test-project"),
	});
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("us-central1-aiplatform.googleapis.com", 443)),
		inputs,
	};
	let req = ::http::Request::builder()
		.uri("/v1/messages")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "claude-haiku-4-5-20251001",
				"max_tokens": 64,
				"messages": [{"role": "user", "content": "say hi"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		upstream_route_type,
		..
	} = provider
		.process_messages_request(&backend_info, None, req, false, &mut None)
		.await
		.expect("Vertex Anthropic messages request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(upstream_route_type, RouteType::Messages);
	assert!(forwarded_json.get("model").is_none());
	assert_eq!(
		forwarded_json["anthropic_version"],
		json!("vertex-2023-10-16")
	);
}

#[tokio::test]
async fn provider_model_is_set_before_llm_transformations() {
	use crate::http::auth::BackendInfo;
	use crate::llm::policy::Policy;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::OpenAI(openai::Provider {
		model: Some("gcp/failover-model".into()),
	});
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.openai.com", 443)),
		inputs,
	};
	let policy = Policy {
		transformations: Some(
			[(
				"model".to_string(),
				std::sync::Arc::new(
					crate::cel::Expression::new_strict(r#"llmRequest.model.stripPrefix("gcp/")"#).unwrap(),
				),
			)]
			.into_iter()
			.collect(),
		),
		..Default::default()
	};
	let req = ::http::Request::builder()
		.uri("/v1/chat/completions")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "public-model",
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_completions_request(&backend_info, Some(&policy), req, false, &mut None)
		.await
		.expect("OpenAI completions request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(forwarded_json["model"], json!("failover-model"));
	assert_eq!(llm_request.request_model, "failover-model");
}

#[tokio::test]
async fn llm_transformations_can_set_missing_model() {
	use crate::http::auth::BackendInfo;
	use crate::llm::policy::Policy;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.openai.com", 443)),
		inputs,
	};
	let policy = Policy {
		transformations: Some(
			[(
				"model".to_string(),
				std::sync::Arc::new(crate::cel::Expression::new_strict(r#""transformed-model""#).unwrap()),
			)]
			.into_iter()
			.collect(),
		),
		..Default::default()
	};
	let req = ::http::Request::builder()
		.uri("/v1/chat/completions")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"messages": [{"role": "user", "content": "hello"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		..
	} = provider
		.process_completions_request(&backend_info, Some(&policy), req, false, &mut None)
		.await
		.expect("OpenAI completions request should process")
	else {
		panic!("expected forwarded request");
	};

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");

	assert_eq!(forwarded_json["model"], json!("transformed-model"));
	assert_eq!(llm_request.request_model, "transformed-model");
}

#[tokio::test]
async fn copilot_anthropic_model_uses_messages_route() {
	use crate::http::auth::BackendInfo;
	use crate::test_helpers::proxymock::setup_proxy_test;
	use crate::types::agent::BackendTarget;

	let provider = AIProvider::Copilot(copilot::Provider { model: None });
	let inputs = setup_proxy_test("{}").unwrap().pi;
	let backend_info = BackendInfo {
		target: BackendTarget::Invalid,
		call_target: Target::from(("api.githubcopilot.com", 443)),
		inputs,
	};
	let req = ::http::Request::builder()
		.uri("/v1/messages")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{
				"model": "claude-sonnet-4",
				"max_tokens": 64,
				"messages": [{"role": "user", "content": "say hi"}]
			}"#
				.to_vec(),
		))
		.unwrap();

	let RequestResult::Success {
		request: forwarded,
		llm_request,
		upstream_route_type,
	} = provider
		.process_messages_request(&backend_info, None, req, false, &mut None)
		.await
		.expect("Copilot Anthropic messages request should process")
	else {
		panic!("expected forwarded request");
	};

	assert_eq!(upstream_route_type, RouteType::Messages);
	assert_eq!(
		llm_request.cache_convention,
		CacheTokenConvention::InputExcludesCache
	);

	let mut setup_req =
		crate::http::tests_common::request("https://example.com/v1/messages", http::Method::POST, &[]);
	provider
		.setup_request(
			&mut setup_req,
			upstream_route_type,
			Some(&llm_request),
			None,
			None,
			false,
		)
		.expect("setup_request should succeed");
	assert_eq!(setup_req.uri().path(), "/v1/messages");

	let forwarded_body = forwarded.collect().await.unwrap().to_bytes();
	let forwarded_json: Value =
		serde_json::from_slice(&forwarded_body).expect("forwarded request should be JSON");
	assert_eq!(forwarded_json["model"], json!("claude-sonnet-4"));
	assert_eq!(forwarded_json["max_tokens"], json!(64));
}

#[test]
fn openai_token_limit_normalization_keeps_explicit_max_completion_tokens() {
	let mut request: types::completions::Request = serde_json::from_value(json!({
		"model": "gpt-5.4",
		"max_tokens": 1024,
		"max_completion_tokens": 2048,
		"messages": [{"role": "user", "content": "hello"}]
	}))
	.expect("valid completions request");

	request.normalize_openai_token_limit();

	assert_eq!(request.max_tokens, None);
	assert_eq!(request.max_completion_tokens, Some(2048));
}

#[test]
fn test_adaptive_thinking_without_effort_maps_to_high_reasoning_effort() {
	let request: types::messages::Request = serde_json::from_value(json!({
		"model": "claude-opus-4-6",
		"max_tokens": 256,
		"thinking": {
			"type": "adaptive"
		},
		"messages": [
			{
				"role": "user",
				"content": "Give one concise insight."
			}
		]
	}))
	.expect("valid messages request");

	let translated = conversion::completions::from_messages::translate(&request)
		.expect("messages->completions translation");
	let translated: Value =
		serde_json::from_slice(&translated).expect("translated request should be valid json");

	assert_eq!(translated.get("reasoning_effort"), Some(&json!("high")));
}

#[test]
fn test_completions_reasoning_effort_maps_to_enabled_thinking_budget() {
	let request: types::completions::Request = serde_json::from_value(json!({
		"model": "claude-opus-4-6",
		"messages": [
			{ "role": "user", "content": "Give one concise insight." }
		],
		"reasoning_effort": "minimal"
	}))
	.expect("valid completions request");

	let translated = conversion::messages::from_completions::translate(&request)
		.expect("completions->messages translation");
	let translated: Value =
		serde_json::from_slice(&translated).expect("translated request should be valid json");

	assert_eq!(
		translated["thinking"],
		json!({
			"type": "enabled",
			"budget_tokens": 1024
		})
	);
	assert!(translated.get("output_config").is_none());
}

#[test]
fn test_completions_json_schema_response_format_maps_to_anthropic_output_config() {
	let request: types::completions::Request = serde_json::from_value(json!({
		"model": "claude-opus-4-6",
		"messages": [
			{ "role": "user", "content": "Return one short summary." }
		],
		"response_format": {
			"type": "json_schema",
			"json_schema": {
				"name": "summary_schema",
				"schema": {
					"type": "object",
					"properties": { "summary": { "type": "string" } },
					"required": ["summary"],
					"additionalProperties": false
				}
			}
		}
	}))
	.expect("valid completions request");

	let translated = conversion::messages::from_completions::translate(&request)
		.expect("completions->messages translation");
	let translated: Value =
		serde_json::from_slice(&translated).expect("translated request should be valid json");

	assert_eq!(
		translated["output_config"]["format"],
		json!({
			"type": "json_schema",
			"schema": {
				"type": "object",
				"properties": { "summary": { "type": "string" } },
				"required": ["summary"],
				"additionalProperties": false
			}
		})
	);
}

#[test]
fn test_messages_output_config_format_maps_to_openai_response_format() {
	let request: types::messages::Request = serde_json::from_value(json!({
		"model": "claude-opus-4-6",
		"max_tokens": 256,
		"output_config": {
			"format": {
				"type": "json_schema",
				"schema": {
					"type": "object",
					"properties": { "answer": { "type": "number" } },
					"required": ["answer"],
					"additionalProperties": false
				}
			}
		},
		"messages": [
			{
				"role": "user",
				"content": "What is 2+2?"
			}
		]
	}))
	.expect("valid messages request");

	let translated = conversion::completions::from_messages::translate(&request)
		.expect("messages->completions translation");
	let translated: Value =
		serde_json::from_slice(&translated).expect("translated request should be valid json");

	assert_eq!(translated["response_format"]["type"], json!("json_schema"));
	assert_eq!(
		translated["response_format"]["json_schema"]["name"],
		json!("structured_output")
	);
	assert_eq!(
		translated["response_format"]["json_schema"]["schema"],
		json!({
			"type": "object",
			"properties": { "answer": { "type": "number" } },
			"required": ["answer"],
			"additionalProperties": false
		})
	);
}

/// Verifies that `process_response` routes a non-success response through
/// the buffered error path even when the request has `streaming: true`.
///
/// Constructs a Bedrock 400 JSON error response and passes it through
/// `process_response` with a streaming `LLMRequest`. Asserts the returned
/// body is non-empty, valid JSON, and preserves the original error message.
#[tokio::test]
async fn process_response_routes_streaming_error_to_buffered_path() {
	use crate::proxy::httpproxy::PolicyClient;
	use crate::test_helpers::proxymock::setup_proxy_test;

	let bedrock = AIProvider::bedrock(bedrock::Provider {
		model: Some(strng::new("anthropic.claude-3-5-sonnet-20241022-v2:0")),
		region: strng::new("us-west-2"),
		guardrail_identifier: None,
		guardrail_version: None,
	});

	let error_json = r#"{"message":"Expected toolResult blocks at messages.2.content for the following Ids: tooluse_abc123"}"#;

	let req = LLMRequest {
		input_tokens: None,
		input_format: InputFormat::Completions,
		cache_convention: CacheTokenConvention::pending(),
		request_model: "input-model".into(),
		provider: Default::default(),
		streaming: true,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	};

	let body = Body::from(error_json.as_bytes().to_vec());
	let mut resp = Response::new(body);
	*resp.status_mut() = ::http::StatusCode::BAD_REQUEST;
	resp.headers_mut().insert(
		::http::header::CONTENT_TYPE,
		"application/json".parse().unwrap(),
	);

	let client = PolicyClient::new(setup_proxy_test("{}").unwrap().pi);

	let result = bedrock
		.process_response(
			client,
			req,
			LLMResponsePolicies::default(),
			None,
			AsyncLog::default(),
			llm::LogContentFields::default(),
			None,
			resp,
		)
		.await
		.expect("process_response should succeed for error responses");

	assert_eq!(result.status(), ::http::StatusCode::BAD_REQUEST);

	let result_body = result.collect().await.unwrap().to_bytes();
	assert!(
		!result_body.is_empty(),
		"error response body must not be empty",
	);

	let parsed: Value =
		serde_json::from_slice(&result_body).expect("translated error should be valid JSON");

	let message = parsed
		.pointer("/error/message")
		.and_then(|v| v.as_str())
		.unwrap_or_default();
	assert!(
		message.contains("toolResult"),
		"translated error should preserve the original message, got: {message}",
	);
}

#[test]
fn openai_completions_error_translates_to_messages_client() {
	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Messages;
	req.request_model = "gpt-4o".into();

	let error = Bytes::from_static(
		br#"{"error":{"message":"bad request","type":"invalid_request_error","param":null,"code":400}}"#,
	);
	let translated = provider
		.process_error(&req, ::http::StatusCode::BAD_REQUEST, &error)
		.expect("OpenAI error should translate to messages error");
	let body: Value = serde_json::from_slice(&translated).expect("translated error should be JSON");

	assert_eq!(body["type"], json!("error"));
	assert_eq!(body["error"]["type"], json!("invalid_request_error"));
	assert_eq!(body["error"]["message"], json!("bad request"));
}

#[test]
fn custom_messages_error_translates_to_completions_client() {
	let provider = custom_provider(custom::ProviderFormat::Messages);
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Completions;
	req.request_model = "claude-test".into();

	let error = Bytes::from_static(
		br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
	);
	let translated = provider
		.process_error(&req, ::http::StatusCode::BAD_REQUEST, &error)
		.expect("Anthropic error should translate to completions error");
	let body: Value = serde_json::from_slice(&translated).expect("translated error should be JSON");

	assert_eq!(body["error"]["type"], json!("invalid_request_error"));
	assert_eq!(body["error"]["message"], json!("bad request"));
}

#[test]
fn foundry_claude_messages_error_uses_anthropic_shape() {
	let provider = AIProvider::azure(azure::Provider {
		model: None,
		resource_name: strng::new("example"),
		resource_type: azure::AzureResourceType::Foundry,
		api_version: None,
		project_name: Some(strng::new("project")),
	});
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Messages;
	req.request_model = "claude-haiku-4-5".into();

	let error = Bytes::from_static(
		br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
	);
	let translated = provider
		.process_error(&req, ::http::StatusCode::BAD_REQUEST, &error)
		.expect("Foundry Claude messages error should stay Anthropic-shaped");
	let body: Value = serde_json::from_slice(&translated).expect("translated error should be JSON");

	assert_eq!(body["type"], json!("error"));
	assert_eq!(body["error"]["type"], json!("invalid_request_error"));
	assert_eq!(body["error"]["message"], json!("bad request"));
}

#[tokio::test]
async fn process_streaming_bedrock_completions_normalizes_sse_headers_and_done() {
	use crate::proxy::httpproxy::PolicyClient;
	use crate::test_helpers::proxymock::setup_proxy_test;
	let bedrock = AIProvider::bedrock(bedrock::Provider {
		model: Some(strng::new("openai.gpt-oss-120b-1:0")),
		region: strng::new("us-east-1"),
		guardrail_identifier: None,
		guardrail_version: None,
	});

	let body = Body::from(
		fs::read(fixture_path("response/bedrock/basic.bin"))
			.expect("failed to read Bedrock streaming fixture"),
	);
	let mut resp = Response::new(body);
	resp.headers_mut().insert(
		::http::header::CONTENT_TYPE,
		"application/vnd.amazon.eventstream".parse().unwrap(),
	);
	resp.headers_mut().insert(
		crate::http::x_headers::X_AMZN_REQUESTID,
		"request_id".parse().unwrap(),
	);

	let client = PolicyClient::new(setup_proxy_test("{}").unwrap().pi);
	let translated = bedrock
		.process_streaming(
			client,
			LLMRequest {
				input_tokens: None,
				input_format: InputFormat::Completions,
				cache_convention: CacheTokenConvention::pending(),
				request_model: "input-model".into(),
				provider: Default::default(),
				streaming: true,
				params: Default::default(),
				prompt: None,
				provider_state: None,
		web_search: None,
			},
			LLMResponsePolicies::default(),
			None,
			AsyncLog::default(),
			llm::LogContentFields::default(),
			None,
			resp,
		)
		.expect("Bedrock streaming translation should succeed");

	crate::http::tests_common::assert_header(
		&translated,
		::http::header::CONTENT_TYPE,
		"text/event-stream",
	);

	let body = translated.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(body.to_vec()).expect("stream should be valid UTF-8");
	assert!(
		text.ends_with("data: [DONE]\n\n"),
		"translated Bedrock completions stream must end with [DONE], got:\n{text}",
	);
	assert!(
		!text.contains("event: \n"),
		"translated Bedrock completions stream must not emit empty event fields:\n{text}",
	);
}

#[test]
fn setup_request_openai_applies_prefixed_path_without_host_override() {
	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let mut req = crate::http::tests_common::request(
		"https://example.com/v1/messages?trace=repro",
		http::Method::POST,
		&[],
	);

	provider
		.setup_request(
			&mut req,
			RouteType::Messages,
			None,
			None,
			Some("/v1/custom"),
			false,
		)
		.expect("setup_request should succeed");

	assert_eq!(
		req.uri().authority().map(|a| a.as_str()),
		Some("api.openai.com")
	);
	assert_eq!(req.uri().path(), "/v1/custom/chat/completions");
	assert_eq!(req.uri().query(), Some("trace=repro"));
}

#[test]
fn setup_request_openai_normalizes_trailing_slash_in_path_prefix() {
	let provider = AIProvider::OpenAI(openai::Provider { model: None });
	let mut req = crate::http::tests_common::request(
		"https://example.com/v1/messages?trace=repro",
		http::Method::POST,
		&[],
	);

	provider
		.setup_request(
			&mut req,
			RouteType::Messages,
			None,
			None,
			Some("/v1/custom/"),
			false,
		)
		.expect("setup_request should succeed");

	assert_eq!(req.uri().path(), "/v1/custom/chat/completions");
	assert_eq!(req.uri().query(), Some("trace=repro"));
}

#[test]
fn setup_request_custom_path_override_wins_over_format_path() {
	let provider = AIProvider::Custom(custom::Provider {
		model: None,
		provider_override: None,
		formats: vec![custom::ProviderFormatConfig {
			format: custom::ProviderFormat::Messages,
			path: Some(strng::literal!("/api/messages")),
		}],
	});
	let llm_request = LLMRequest {
		input_tokens: None,
		input_format: InputFormat::Completions,
		cache_convention: CacheTokenConvention::pending(),
		request_model: "input-model".into(),
		provider: Default::default(),
		streaming: false,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	};
	let mut req = crate::http::tests_common::request(
		"https://proxy.example.com/v1/chat/completions?trace=repro",
		http::Method::POST,
		&[],
	);

	provider
		.setup_request(
			&mut req,
			RouteType::Completions,
			Some(&llm_request),
			Some("/override/messages"),
			None,
			true,
		)
		.expect("setup_request should succeed");

	assert_eq!(req.uri().path(), "/override/messages");
	assert_eq!(req.uri().query(), None);
}

fn llm_request_for_path(request_model: &str) -> LLMRequest {
	LLMRequest {
		input_tokens: None,
		input_format: InputFormat::Messages,
		cache_convention: CacheTokenConvention::pending(),
		request_model: request_model.into(),
		provider: Default::default(),
		streaming: false,
		params: Default::default(),
		prompt: None,
		provider_state: None,
		web_search: None,
	}
}

fn assert_prefixed_host_override_path(
	provider: AIProvider,
	request_model: &str,
	expected_path: &str,
	expected_query: Option<&str>,
) {
	let llm_request = llm_request_for_path(request_model);
	let mut req = crate::http::tests_common::request(
		"https://proxy.example.com/v1/messages?trace=repro",
		http::Method::POST,
		&[],
	);

	provider
		.setup_request(
			&mut req,
			RouteType::Messages,
			Some(&llm_request),
			None,
			Some("/proxy/"),
			true,
		)
		.expect("setup_request should succeed");

	assert_eq!(req.uri().path(), expected_path);
	assert_eq!(req.uri().query(), expected_query);
}

#[test]
fn setup_request_gemini_applies_path_prefix_with_host_override() {
	assert_prefixed_host_override_path(
		AIProvider::Gemini(gemini::Provider { model: None }),
		"gemini-2.5-pro",
		"/proxy/v1beta/openai/chat/completions",
		Some("trace=repro"),
	);
}

#[test]
fn setup_request_vertex_applies_path_prefix_with_host_override() {
	assert_prefixed_host_override_path(
		AIProvider::Vertex(vertex::Provider {
			model: None,
			region: Some(strng::new("us-central1")),
			project_id: strng::new("example-project"),
		}),
		"gemini-2.5-pro",
		"/proxy/v1/projects/example-project/locations/us-central1/endpoints/openapi/chat/completions",
		Some("trace=repro"),
	);
}

#[test]
fn setup_request_bedrock_applies_path_prefix_with_host_override() {
	assert_prefixed_host_override_path(
		AIProvider::bedrock(bedrock::Provider {
			model: None,
			region: strng::new("us-east-1"),
			guardrail_identifier: None,
			guardrail_version: None,
		}),
		"anthropic.claude-3-5-sonnet-20241022-v2:0",
		"/proxy/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse",
		Some("trace=repro"),
	);
}

#[test]
fn setup_request_azure_applies_path_prefix_with_host_override() {
	assert_prefixed_host_override_path(
		AIProvider::azure(azure::Provider {
			model: None,
			resource_name: strng::new("example"),
			resource_type: azure::AzureResourceType::OpenAI,
			api_version: Some(strng::new("2024-02-15-preview")),
			project_name: None,
		}),
		"gpt-4.1",
		"/proxy/openai/deployments/gpt-4.1/chat/completions",
		Some("api-version=2024-02-15-preview&trace=repro"),
	);
}

#[test]
fn completions_response_missing_message_and_usage_fields() {
	// Gemini's OpenAI-compat endpoint can omit `message` from choices and
	// `completion_tokens` from usage. Verify deserialization succeeds with defaults.
	let json = r#"{
		"id": "1",
		"object": "chat.completion",
		"created": 0,
		"model": "google/gemini-2.5-flash",
		"choices": [{"index": 0, "finish_reason": "length"}],
		"usage": {"prompt_tokens": 5, "total_tokens": 12}
	}"#;
	let resp: types::completions::Response = serde_json::from_str(json).unwrap();
	assert_eq!(resp.choices.len(), 1);
	assert_eq!(resp.choices[0].message.content, None);
	assert_eq!(resp.choices[0].message.role, None);
	let usage = resp.usage.unwrap();
	assert_eq!(usage.prompt_tokens, 5);
	assert_eq!(usage.completion_tokens, 0);
	assert_eq!(usage.total_tokens, 12);
}

#[test]
fn completions_to_messages_response_allows_missing_openai_metadata() {
	let body = Bytes::from_static(
		br#"{
			"id": "chatcmpl-1",
			"model": "gpt-5-mini",
			"choices": [{
				"message": {"role": "assistant", "content": "hi"},
				"finish_reason": "stop"
			}],
			"usage": {
				"completion_tokens": 16,
				"prompt_tokens": 9,
				"prompt_tokens_details": {"cached_tokens": 0},
				"total_tokens": 25
			},
			"copilot_usage": {
				"token_details": []
			}
		}"#,
	);

	conversion::completions::from_messages::translate_response(&body)
		.expect("messages response translation should not require OpenAI metadata");
}

#[tokio::test]
async fn bedrock_from_messages_stream_captures_completion() {
	let input_bytes =
		fs::read(fixture_path("response/bedrock/basic.bin")).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
			provider: "bedrock".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::bedrock::from_messages::translate_stream(
		body,
		buffer_limit,
		logger,
		"us.anthropic.claude-haiku-4-5-20251001-v1:0",
		"msg_123",
		llm::LogContentFields {
			completion: true,
			tool_calls: true,
		},
		None,
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	let completion = info
		.response
		.completion
		.expect("completion should be set for bedrock streaming");
	assert!(
		!completion.join("").is_empty(),
		"completion should contain response text"
	);
}

#[tokio::test]
async fn bedrock_from_messages_stream_skips_completion_when_disabled() {
	let input_bytes =
		fs::read(fixture_path("response/bedrock/basic.bin")).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
			provider: "bedrock".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::bedrock::from_messages::translate_stream(
		body,
		buffer_limit,
		logger,
		"us.anthropic.claude-haiku-4-5-20251001-v1:0",
		"msg_123",
		llm::LogContentFields::default(),
		None,
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	assert!(
		info.response.completion.is_none(),
		"completion should not be set when log_content.completion is false"
	);
	assert!(
		info.response.output_messages.is_none(),
		"output messages should not be set when log_content.tool_calls is false"
	);
}

#[tokio::test]
async fn bedrock_from_messages_stream_captures_tool_calls() {
	let input_bytes =
		fs::read(fixture_path("response/bedrock/tool.bin")).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
			provider: "bedrock".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let body = conversion::bedrock::from_messages::translate_stream(
		body,
		1024 * 1024,
		logger,
		"us.anthropic.claude-haiku-4-5-20251001-v1:0",
		"msg_123",
		llm::LogContentFields {
			completion: false,
			tool_calls: true,
		},
		None,
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	assert!(info.response.completion.is_none());
	let output_messages = info
		.response
		.output_messages
		.expect("output messages should be set for Bedrock tool calls");
	assert_eq!(
		output_messages[0].finish_reason.as_deref(),
		Some("tool_use")
	);
	let tool_calls = output_messages[0].tool_calls();
	assert_eq!(tool_calls.len(), 2);
	assert_eq!(tool_calls[0].name.as_str(), "top_song");
	assert_eq!(tool_calls[0].arguments, serde_json::json!({"sign": "WZPZ"}));
	assert_eq!(tool_calls[1].name.as_str(), "hello");
	assert_eq!(
		tool_calls[1].arguments,
		serde_json::json!({"sign": "world"})
	);
}

#[tokio::test]
async fn messages_passthrough_stream_captures_completion() {
	let input_path = fixture_path("response/anthropic/stream_basic.json");
	let input_bytes = fs::read(&input_path).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "claude-haiku-4-5-20251001".into(),
			provider: "anthropic".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::messages::passthrough_stream(
		body,
		buffer_limit,
		logger,
		llm::LogContentFields {
			completion: true,
			tool_calls: true,
		},
	);
	// Consume the body to drive the stream to completion
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	let completion = info
		.response
		.completion
		.expect("completion should be set for messages streaming");
	assert_eq!(
		completion.join(""),
		"Hi there! How are you doing today? Is there anything I can help you with?"
	);
}

#[tokio::test]
async fn messages_passthrough_stream_skips_completion_when_disabled() {
	let input_path = fixture_path("response/anthropic/stream_basic.json");
	let input_bytes = fs::read(&input_path).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "claude-haiku-4-5-20251001".into(),
			provider: "anthropic".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::messages::passthrough_stream(
		body,
		buffer_limit,
		logger,
		llm::LogContentFields::default(),
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	assert!(
		info.response.completion.is_none(),
		"completion should not be set when log_content.completion is false"
	);
	assert!(
		info.response.output_messages.is_none(),
		"output messages should not be set when log_content.tool_calls is false"
	);
}

#[tokio::test]
async fn messages_passthrough_stream_captures_tool_calls() {
	let input_bytes =
		fs::read(fixture_path("response/anthropic/stream_tool.json")).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Messages,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "claude-haiku-4-5-20251001".into(),
			provider: "anthropic".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let body = conversion::messages::passthrough_stream(
		body,
		1024 * 1024,
		logger,
		llm::LogContentFields {
			completion: false,
			tool_calls: true,
		},
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	assert!(info.response.completion.is_none());
	let output_messages = info
		.response
		.output_messages
		.expect("output messages should be set for Anthropic tool calls");
	assert_eq!(
		output_messages[0].finish_reason.as_deref(),
		Some("tool_use")
	);
	let tool_calls = output_messages[0].tool_calls();
	assert_eq!(tool_calls.len(), 1);
	assert_eq!(tool_calls[0].id.as_str(), "toolu_01A");
	assert_eq!(tool_calls[0].name.as_str(), "get_weather");
	assert_eq!(
		tool_calls[0].arguments,
		serde_json::json!({"location": "San Francisco"})
	);
}

#[tokio::test]
async fn responses_passthrough_stream_captures_completion_and_tool_calls() {
	let input_path = fixture_path("response/responses/stream.json");
	let input_bytes = fs::read(&input_path).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Responses,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "gpt-4.1-mini".into(),
			provider: "openai".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::responses::passthrough_stream(
		body,
		buffer_limit,
		logger,
		llm::LogContentFields {
			completion: true,
			tool_calls: true,
		},
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	let completion = info
		.response
		.completion
		.expect("completion should be set for responses streaming");
	assert_eq!(completion.join(""), "Hello");
	let output_messages = info
		.response
		.output_messages
		.expect("output messages should be set for responses streaming");
	assert_eq!(
		output_messages[0].finish_reason.as_deref(),
		Some("completed")
	);
	let tool_calls = output_messages[0].tool_calls();
	assert_eq!(tool_calls.len(), 1);
	assert_eq!(tool_calls[0].id.as_str(), "call_xxx");
	assert_eq!(tool_calls[0].name.as_str(), "get_weather");
	assert_eq!(
		tool_calls[0].arguments,
		serde_json::json!({"location": "San Francisco"})
	);
}

#[tokio::test]
async fn responses_passthrough_stream_skips_completion_when_disabled() {
	let input_path = fixture_path("response/responses/stream.json");
	let input_bytes = fs::read(&input_path).expect("Failed to read fixture");
	let body = Body::from(input_bytes);
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format: InputFormat::Responses,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "gpt-4.1-mini".into(),
			provider: "openai".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: None,
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	let buffer_limit = 1024 * 1024;
	let body = conversion::responses::passthrough_stream(
		body,
		buffer_limit,
		logger,
		llm::LogContentFields::default(),
	);
	let _ = body.collect().await.unwrap();
	let info = log2
		.take()
		.expect("log should have LLMInfo after stream completes");
	assert!(
		info.response.completion.is_none(),
		"completion should not be set when log_content.completion is false"
	);
	assert!(
		info.response.output_messages.is_none(),
		"output messages should not be set when log_content.tool_calls is false"
	);
}

fn vertex_provider(model: &str) -> AIProvider {
	AIProvider::Vertex(vertex::Provider {
		model: Some(strng::new(model)),
		region: None,
		project_id: strng::new("test-project"),
	})
}

fn custom_provider(format: custom::ProviderFormat) -> AIProvider {
	AIProvider::Custom(custom::Provider {
		model: None,
		provider_override: None,
		formats: vec![custom::ProviderFormatConfig { format, path: None }],
	})
}

#[tokio::test]
async fn read_body_decodes_gzip_request_before_json_parse() {
	// Regression: a gzip-compressed request body (Content-Encoding: gzip) must be
	// decompressed before the JSON parse. Clients such as the Claude Code harness
	// gzip request bodies above a size threshold; previously the reader handed the
	// raw compressed bytes to serde_json and failed with a misleading
	// "LLM request body must be valid JSON" 400, even for tiny payloads.
	let provider = custom_provider(custom::ProviderFormat::Messages);

	let plaintext =
		br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
	let gz = crate::http::compression::encode_body(plaintext, "gzip")
		.await
		.expect("gzip encode");
	// The payload is genuinely compressed (gzip magic) and tiny, so this exercises
	// content-encoding decoding rather than the buffer-size path.
	assert_eq!(&gz[..2], &[0x1f, 0x8b]);

	let req = ::http::Request::builder()
		.uri("/v1/messages")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.header(::http::header::CONTENT_ENCODING, "gzip")
		.body(Body::from(gz.to_vec()))
		.unwrap();

	let (parts, parsed) = provider
		.read_body_and_default_model::<types::messages::Request>(None, req, &mut None)
		.await
		.expect("gzip request body should decode and parse as JSON");

	assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-4-5"));
	// The encoding header is stripped now that the body is plaintext.
	assert!(
		parts
			.headers
			.get(::http::header::CONTENT_ENCODING)
			.is_none()
	);
}

#[tokio::test]
async fn read_body_still_parses_plaintext_request() {
	// A plaintext (unencoded) request body must continue to parse unchanged — the
	// decompression path is a no-op when no Content-Encoding is present.
	let provider = custom_provider(custom::ProviderFormat::Messages);

	let req = ::http::Request::builder()
		.uri("/v1/messages")
		.header(::http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(
			br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#
				.to_vec(),
		))
		.unwrap();

	let (_parts, parsed) = provider
		.read_body_and_default_model::<types::messages::Request>(None, req, &mut None)
		.await
		.expect("plaintext request body should parse as JSON");

	assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-4-5"));
}

#[test]
fn custom_provider_name_falls_back_to_custom() {
	let provider = custom_provider(custom::ProviderFormat::Completions);
	assert_eq!(provider.provider(), strng::literal!("custom"));
}

#[test]
fn custom_provider_override_drives_provider_name() {
	let provider = AIProvider::Custom(custom::Provider {
		model: None,
		provider_override: Some(strng::literal!("cohere")),
		formats: vec![custom::ProviderFormatConfig {
			format: custom::ProviderFormat::Rerank,
			path: None,
		}],
	});
	assert_eq!(provider.provider(), strng::literal!("cohere"));
}

#[test]
fn vertex_anthropic_model_uses_exclusive_convention() {
	let provider = vertex_provider("anthropic/claude-sonnet-4-5");
	assert_eq!(
		cache_convention_for(&provider, None, "anthropic/claude-sonnet-4-5"),
		CacheTokenConvention::InputExcludesCache,
	);
}

#[test]
fn vertex_non_anthropic_model_uses_inclusive_convention() {
	let provider = vertex_provider("gemini-2.0-flash");
	assert_eq!(
		cache_convention_for(&provider, None, "gemini-2.0-flash"),
		CacheTokenConvention::InputIncludesCache,
	);
}

#[test]
fn custom_messages_backend_uses_exclusive_convention() {
	let provider = custom_provider(custom::ProviderFormat::Messages);
	assert_eq!(
		cache_convention_for(
			&provider,
			Some(custom::ProviderFormat::Messages),
			"some-model"
		),
		CacheTokenConvention::InputExcludesCache,
	);
}

#[test]
fn custom_completions_backend_uses_inclusive_convention() {
	let provider = custom_provider(custom::ProviderFormat::Completions);
	assert_eq!(
		cache_convention_for(
			&provider,
			Some(custom::ProviderFormat::Completions),
			"some-model"
		),
		CacheTokenConvention::InputIncludesCache,
	);
}

#[test]
fn fixed_providers_classify_by_family() {
	assert_eq!(
		cache_convention_for(
			&AIProvider::Anthropic(anthropic::Provider { model: None }),
			None,
			"claude-sonnet-4-5"
		),
		CacheTokenConvention::InputExcludesCache,
	);
	assert_eq!(
		cache_convention_for(
			&AIProvider::OpenAI(openai::Provider { model: None }),
			Some(custom::ProviderFormat::Completions),
			"gpt-4o"
		),
		CacheTokenConvention::InputIncludesCache,
	);
}

/// An upstream error body that is not JSON must still reach the client in the
/// client's own error shape, with the upstream status intact.
///
/// Regression, measured on the dev pilot 2026-09-04: a `/v1/messages` request
/// whose upstream answered a plain nginx `404` HTML page came back as
/// `503 processing failed: failed to parse response`. `translate_error` parses
/// the error body to reshape it, the parse failed, and the resulting
/// `AIError::ResponseParsing` became `ProxyError::Processing` -- which is a
/// 503, is not `is_retryable()`, and is what the health policy's
/// `unhealthyCondition` is then evaluated against. So one un-parseable byte
/// costs the client a correct error body AND costs the route both halves of
/// failover: retry sees an `Err` it will not replay, and health never sees the
/// real upstream code. Only the two passthrough arms of `ChatTranslation::error`
/// (`Completions`/`Responses` against an OpenAI-shaped upstream) were immune.
#[test]
fn non_json_upstream_error_synthesizes_messages_error() {
	let provider = custom_provider(custom::ProviderFormat::Completions);
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Messages;
	req.request_model = "glm-5.2".into();

	let error = Bytes::from_static(
		b"<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n",
	);
	let translated = provider
		.process_error(&req, ::http::StatusCode::NOT_FOUND, &error)
		.expect("a non-JSON upstream error must not fail the exchange");
	let body: Value = serde_json::from_slice(&translated).expect("synthesized error should be JSON");

	assert_eq!(body["type"], json!("error"));
	assert_eq!(body["error"]["type"], json!("not_found_error"));
	let message = body["error"]["message"].as_str().unwrap_or_default();
	assert!(
		message.contains("404 Not Found"),
		"synthesized message should carry the upstream body so the failure is diagnosable, got: {message}",
	);
}

/// The same for the Google error arm, which a Completions client reaches
/// through a Gemini or Vertex backend. `parse_google_error` was strict for the
/// same reason and had the same consequence.
#[test]
fn non_json_upstream_error_synthesizes_completions_error() {
	let provider = AIProvider::Gemini(gemini::Provider { model: None });
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Completions;
	req.request_model = "gemini-2.5-pro".into();

	let error = Bytes::from_static(b"upstream connect error or disconnect/reset before headers");
	let translated = provider
		.process_error(&req, ::http::StatusCode::BAD_GATEWAY, &error)
		.expect("a non-JSON upstream error must not fail the exchange");
	let body: Value = serde_json::from_slice(&translated).expect("synthesized error should be JSON");

	assert_eq!(body["error"]["type"], json!("api_error"));
	let message = body["error"]["message"].as_str().unwrap_or_default();
	assert!(
		message.contains("upstream connect error"),
		"synthesized message should carry the upstream body, got: {message}",
	);
}

/// A well-formed upstream error must still be translated, not synthesized --
/// the fallback may not swallow the upstream's own `type` and `message`.
#[test]
fn json_upstream_error_is_still_translated_not_synthesized() {
	let provider = custom_provider(custom::ProviderFormat::Completions);
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Messages;

	let error = Bytes::from_static(
		br#"{"error":{"message":"model not found","type":"model_error","param":null,"code":404}}"#,
	);
	let translated = provider
		.process_error(&req, ::http::StatusCode::NOT_FOUND, &error)
		.expect("well-formed error should translate");
	let body: Value = serde_json::from_slice(&translated).expect("translated error should be JSON");

	assert_eq!(body["error"]["type"], json!("model_error"));
	assert_eq!(body["error"]["message"], json!("model not found"));
}

/// The property failover actually depends on: `process_response` must return
/// `Ok` with the upstream status preserved, so `should_retry` can match it
/// against `traffic.retry.codes` and the health policy's `unhealthyCondition`
/// can see the real code.
#[tokio::test]
async fn non_json_upstream_error_preserves_status_for_retry_and_health() {
	use crate::proxy::httpproxy::PolicyClient;
	use crate::test_helpers::proxymock::setup_proxy_test;

	let provider = custom_provider(custom::ProviderFormat::Completions);
	let mut req = llm_request_with_tokens(None);
	req.input_format = InputFormat::Messages;
	req.streaming = false;

	let body = Body::from(
		b"<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n</body>\r\n</html>\r\n"
			.to_vec(),
	);
	let mut resp = Response::new(body);
	*resp.status_mut() = ::http::StatusCode::NOT_FOUND;
	resp
		.headers_mut()
		.insert(::http::header::CONTENT_TYPE, "text/html".parse().unwrap());

	let client = PolicyClient::new(setup_proxy_test("{}").unwrap().pi);
	let result = provider
		.process_response(
			client,
			req,
			LLMResponsePolicies::default(),
			None,
			AsyncLog::default(),
			llm::LogContentFields::default(),
			None,
			resp,
		)
		.await
		.expect("process_response must not fail the exchange on a non-JSON error body");

	assert_eq!(
		result.status(),
		::http::StatusCode::NOT_FOUND,
		"the upstream status must survive; retry and health are both evaluated against it",
	);

	let result_body = result.collect().await.unwrap().to_bytes();
	let parsed: Value =
		serde_json::from_slice(&result_body).expect("synthesized error should be valid JSON");
	assert_eq!(parsed["type"], json!("error"));
}

// ---------------------------------------------------------------------------
// G3 web-search sidecar marker bridge (FR-2.6) — probes P13/P16/P17.
//
// These tests exercise `agent_llm::conversion::web_search::translate_stream`,
// the response-side reshape of a sidecar SSE stream into the client's own
// protocol. They live here (not in agent-llm's own test module) because the
// real log-capture harness — `AmendOnDrop` + `test_helpers::policy_client()` —
// is agentgateway-crate; agent-llm's `StreamingUsageGuard::default()` is a
// Noop that silently discards all `update()` calls, so a P17 metering
// assertion run there would pass vacuously.
// ---------------------------------------------------------------------------

/// Build a sidecar SSE body from a sequence of `data:` JSON payloads (each
/// already a JSON string) + a trailing `[DONE]`. Matches the sidecar contract
/// (data:-only, no `event:` field).
fn sidecar_sse(frames: &[&str]) -> Body {
	let mut out = String::new();
	for f in frames {
		out.push_str("data: ");
		out.push_str(f);
		out.push_str("\n\n");
	}
	out.push_str("data: [DONE]\n\n");
	Body::from(out.into_bytes())
}

/// Standard log-capture harness: an `AsyncLog` + `AmendOnDrop` logger wired to
/// a no-op policy client, with `llmresp` stored. Returns `(log2, logger)` so
/// the caller can drain the body then `log2.take()` the final `LLMInfo`.
fn ws_log_harness(input_format: InputFormat) -> (AsyncLog<llm::LLMInfo>, agent_llm::StreamingUsageGuard) {
	let log = AsyncLog::default();
	let log2 = log.clone();
	let llmresp = LLMInfo {
		request: LLMRequest {
			input_tokens: None,
			input_format,
			cache_convention: CacheTokenConvention::pending(),
			request_model: "test-model".into(),
			provider: "test-provider".into(),
			streaming: true,
			params: Default::default(),
			prompt: None,
			provider_state: None,
			web_search: Some(WebSearchStreamContext {
				streaming: true,
				client_tools: true,
			}),
		},
		response: LLMResponse::default(),
	};
	log.store(Some(llmresp));
	let logger = AmendOnDrop::new(
		log,
		LLMResponsePolicies::default(),
		None,
		None,
		crate::test_helpers::policy_client(),
	)
	.into_llm();
	(log2, logger)
}

const WS_TEXT_DELTA: &str =
	r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hello world"}}]}"#;
const WS_TOOL_ROUND: &str =
	r#"{"x-ai-ws-tool-round":{"tool_use_id":"t1","tool_name":"web_search","input":{"query":"rust async"}}}"#;
const WS_USAGE: &str =
	r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}"#;
const WS_PROVENANCE: &str =
	r#"{"x-ai-ws-provenance":{"rounds":[],"citations":[{"n":1,"url":"https://rust-lang.org","title":"Rust"}],"route_type":"llm/v1/chat"}}"#;
const WS_IN_STREAM_ERROR: &str = r#"{"error":{"message":"sidecar blew up"}}"#;

/// P13: augmentation transparency — a rerouted stream reshapes text deltas +
/// tool-round markers + provenance into the client's protocol without dropping
/// the assistant text, and the tool-round (executed server-side) is invisible
/// to the chat client (dropped, not emitted as a broken chunk).
#[tokio::test]
async fn p13_chat_shaper_augmentation_is_transparent() {
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_TOOL_ROUND, WS_USAGE, WS_PROVENANCE]);
	let (log2, logger) = ws_log_harness(InputFormat::Completions);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Completions,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");

	// The assistant text survives the reshape.
	assert!(
		text.contains("hello world"),
		"chat shaper must pass the text delta through; got:\n{text}"
	);
	// The executed tool-round marker is NOT forwarded to the chat client
	// (it would arrive as a malformed chunk the client can't classify).
	assert!(
		!text.contains("x-ai-ws-tool-round"),
		"chat shaper must drop executed tool-round markers; got:\n{text}"
	);
	// Provenance surfaces as a sources delta.content.
	assert!(
		text.contains("rust-lang.org"),
		"chat shaper must emit provenance sources; got:\n{text}"
	);
	// Chat frames are data:-only (no `event:` field — the sidecar speaks chat,
	// and the chat client expects chat-shaped SSE).
	assert!(
		!text.contains("event: "),
		"chat shaper must not emit SSE event: fields; got:\n{text}"
	);
	// Stream terminates with [DONE].
	assert!(
		text.ends_with("data: [DONE]\n\n"),
		"chat shaper must end with [DONE]; got:\n{text}"
	);
	// P17 (usage marker path): the sidecar's usage was recorded.
	let info = log2.take().expect("log should have LLMInfo");
	assert_eq!(info.response.input_tokens, Some(10));
	assert_eq!(info.response.output_tokens, Some(5));
	assert_eq!(info.response.total_tokens, Some(15));
	assert_eq!(info.response.cached_input_tokens, Some(3));
	assert_eq!(info.response.reasoning_tokens, Some(2));
}

/// P13 (Anthropic): the messages shaper drives the full Anthropic event
/// lifecycle — `message_start`, text content-block lifecycle, and a terminal
/// `message_delta`+`message_stop` — from sidecar markers.
#[tokio::test]
async fn p13_messages_shaper_drives_anthropic_lifecycle() {
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_USAGE, WS_PROVENANCE]);
	let (log2, logger) = ws_log_harness(InputFormat::Messages);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Messages,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");

	assert!(
		text.contains("event: message_start\ndata: "),
		"messages shaper must open with message_start; got:\n{text}"
	);
	assert!(
		text.contains("event: content_block_start\ndata: "),
		"messages shaper must open a text content block; got:\n{text}"
	);
	assert!(
		text.contains("\"text\":\"hello world\""),
		"messages shaper must emit the text delta; got:\n{text}"
	);
	assert!(
		text.contains("event: content_block_stop\ndata: "),
		"messages shaper must close the text block; got:\n{text}"
	);
	assert!(
		text.contains("event: message_delta\ndata: "),
		"messages shaper must emit the terminal message_delta; got:\n{text}"
	);
	assert!(
		text.contains("event: message_stop\ndata: "),
		"messages shaper must emit message_stop; got:\n{text}"
	);
	// Citations from provenance ride on a citations_delta.
	assert!(
		text.contains("citations_delta"),
		"messages shaper must emit citations_delta from provenance; got:\n{text}"
	);
	// P17: usage recorded on the LLMInfo. Anthropic input_tokens = prompt -
	// cached (OpenAI prompt_tokens INCLUDES cached; Anthropic reports
	// non-cached + a separate cache_read_input_tokens).
	let info = log2.take().expect("log should have LLMInfo");
	assert_eq!(info.response.input_tokens, Some(10));
	assert_eq!(info.response.output_tokens, Some(5));
	assert_eq!(info.response.total_tokens, Some(15));
}

/// P16: an in-stream sidecar error (`{"error":{"message":...}}`) is surfaced
/// per protocol, never laundered into an empty success. Chat emits an error
/// frame; Responses emits `response.failed`; Messages surfaces it via the
/// terminal stop_reason (no clean `end_turn`).
#[tokio::test]
async fn p16_in_stream_error_is_surfaced_not_laundered() {
	// Chat: the error frame is forwarded, then [DONE].
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_IN_STREAM_ERROR]);
	let (_log2, logger) = ws_log_harness(InputFormat::Completions);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Completions,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");
	assert!(
		text.contains(r#""error":{"message":"sidecar blew up"}"#),
		"chat shaper must surface the in-stream error frame; got:\n{text}"
	);

	// Responses: the in-stream error becomes a response.failed terminal event.
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_IN_STREAM_ERROR]);
	let (_log2, logger) = ws_log_harness(InputFormat::Responses);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Responses,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");
	assert!(
		text.contains("response.failed"),
		"responses shaper must emit response.failed for an in-stream error; got:\n{text}"
	);
	assert!(
		text.contains("sidecar blew up"),
		"responses shaper must carry the error message; got:\n{text}"
	);

	// Messages: the in-stream error surfaces as a terminal message_delta with
	// stop_reason absent (not a clean end_turn). The stream still closes with
	// message_stop (no empty success).
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_IN_STREAM_ERROR]);
	let (_log2, logger) = ws_log_harness(InputFormat::Messages);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Messages,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");
	assert!(
		text.contains("event: message_delta\ndata: "),
		"messages shaper must still emit a terminal message_delta on error; got:\n{text}"
	);
	assert!(
		text.contains("\"stop_reason\":null"),
		"messages shaper must surface the in-stream error as stop_reason=null (not end_turn); got:\n{text}"
	);
	assert!(
		!text.contains("\"stop_reason\":\"end_turn\""),
		"messages shaper must NOT launder an in-stream error into a clean end_turn; got:\n{text}"
	);
}

/// P17: metering on the bypass-equivalent path. When the sidecar sends text
/// deltas but NO `usage` marker, the bridge falls back to ceil(text_chars/4)
/// for output_tokens. When the `usage` marker IS present, it is authoritative.
/// (The true bypass path — web_search configured but not triggered — never
/// reaches this bridge; that path keeps normal upstream accounting. This test
/// covers the bridge's own metering for the no-usage-marker case.)
#[tokio::test]
async fn p17_metering_usage_marker_and_eof_fallback() {
	// Authoritative: usage marker present → exact tokens recorded.
	let body = sidecar_sse(&[WS_TEXT_DELTA, WS_USAGE]);
	let (log2, logger) = ws_log_harness(InputFormat::Completions);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Completions,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let _ = body.collect().await.unwrap();
	let info = log2.take().expect("log should have LLMInfo");
	assert_eq!(
		info.response.output_tokens,
		Some(5),
		"usage marker must be authoritative for output_tokens"
	);

	// Fallback: no usage marker, text present → ceil(text_chars/4).
	// "hello world" = 11 chars → ceil(11/4) = 3.
	let body = sidecar_sse(&[WS_TEXT_DELTA]);
	let (log2, logger) = ws_log_harness(InputFormat::Completions);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Completions,
		WebSearchStreamContext {
			streaming: true,
			client_tools: true,
		},
	);
	let _ = body.collect().await.unwrap();
	let info = log2.take().expect("log should have LLMInfo");
	assert_eq!(
		info.response.output_tokens,
		Some(3),
		"EOF fallback must estimate output_tokens = ceil(text_chars/4) = 3 for 11 chars"
	);
}

/// P13 (client_tools filter): when `client_tools=false`, the sidecar's
/// forwarded client tool-call suspension signal is stripped (the client asked
/// us not to forward its own function tools).
#[tokio::test]
async fn p13_client_tools_false_strips_forwarded_tool_calls() {
	let tool_call_frame = r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
	let body = sidecar_sse(&[WS_TEXT_DELTA, tool_call_frame, WS_USAGE]);
	let (_log2, logger) = ws_log_harness(InputFormat::Completions);
	let body = agent_llm::conversion::web_search::translate_stream(
		body,
		1024 * 1024,
		logger,
		"test-model".to_string(),
		InputFormat::Completions,
		WebSearchStreamContext {
			streaming: true,
			client_tools: false,
		},
	);
	let out = body.collect().await.unwrap().to_bytes();
	let text = String::from_utf8(out.to_vec()).expect("stream must be valid UTF-8");
	assert!(
		!text.contains("tool_calls"),
		"client_tools=false must strip the forwarded tool-call suspension; got:\n{text}"
	);
	// The text delta still passes through.
	assert!(
		text.contains("hello world"),
		"text must still pass through with client_tools=false; got:\n{text}"
	);
}

/// `unparseable_upstream_body` lives in `agent_llm::conversion`; its tests live
/// here because `cargo test -p agent-llm` does not build on this branch (stale
/// `golden_tests.rs`), so an in-crate test module would never run.
mod unparseable_upstream_body {
	use agent_llm::conversion::unparseable_upstream_body;
	use bytes::Bytes;

	#[test]
	fn collapses_whitespace_of_an_html_error_page() {
		let body = Bytes::from_static(
			b"<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n</body>\r\n</html>\r\n",
		);
		assert_eq!(
			unparseable_upstream_body(&body),
			"<html> <head><title>404 Not Found</title></head> <body> </body> </html>"
		);
	}

	#[test]
	fn caps_a_large_error_page() {
		let body = Bytes::from(vec![b'x'; 4096]);
		let out = unparseable_upstream_body(&body);
		assert!(out.ends_with("... (truncated)"), "got: {out}");
		assert_eq!(out.chars().filter(|c| *c == 'x').count(), 512);
	}

	#[test]
	fn describes_an_empty_body() {
		assert_eq!(
			unparseable_upstream_body(&Bytes::new()),
			"upstream returned an empty error body"
		);
	}

	#[test]
	fn does_not_panic_on_invalid_utf8() {
		let body = Bytes::from_static(&[0xff, 0xfe, b'o', b'k']);
		assert!(unparseable_upstream_body(&body).contains("ok"));
	}
}
