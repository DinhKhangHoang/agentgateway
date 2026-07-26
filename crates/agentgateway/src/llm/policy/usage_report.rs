use std::collections::BTreeMap;

use ::http::header::CONTENT_TYPE;
use ::http::{HeaderValue, StatusCode};
use serde::Serialize;

use crate::cel::LLMContext;
use crate::http::filters::BackendRequestTimeout;
use crate::proxy::httpproxy::PolicyClient;
use crate::telemetry::metrics::{OutboundCallKind, OutboundCallSubtype};
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
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		with = "serde_dur_option"
	)]
	#[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
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

#[derive(Debug, Default, Clone, Serialize)]
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
				// Rendered with the same formatter `CelDuration`'s Serialize impl
				// uses, so the string matches what access logs emit (`0.412s`).
				time_to_first_token: ctx
					.time_to_first_token
					.as_ref()
					.and_then(|d| ::cel::format_duration(&d.0)),
				time_per_output_token: ctx
					.time_per_output_token
					.as_ref()
					.and_then(|d| ::cel::format_duration(&d.0)),
			},
			dimensions,
		}
	}
}

/// Maximum concurrent in-flight usage reports per gateway.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 1024;

/// Caps concurrent in-flight usage reports so a slow or wedged receiver
/// cannot pile up unbounded tasks on the gateway.
///
/// The existing token amend spawns one unbounded task per request
/// (`remoteratelimit.rs:145`); this cap exists so that adding a third sink to
/// that fan-out does not inherit the same pile-up under a wedged receiver.
#[derive(Clone)]
pub struct InFlightLimiter(Arc<tokio::sync::Semaphore>);

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

impl std::fmt::Debug for InFlightLimiter {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("InFlightLimiter")
			.field("available", &self.0.available_permits())
			.finish()
	}
}

/// Deliver one usage report, retrying on failure.
///
/// Never returns an error: the response has already reached the client, so
/// there is nothing a caller could do with one. Failures are recorded on
/// `llm_usage_report_dropped` instead, which is the metric operators alert on.
///
/// This is a plain `async fn` rather than something that spawns internally, so
/// tests can await a delivery to completion. Spawning happens at the call site.
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
) -> anyhow::Result<StatusCode> {
	let body = serde_json::to_vec(payload)?;
	let mut req = ::http::Request::builder()
		.method(::http::Method::POST)
		.uri(cfg.path.as_deref().unwrap_or(DEFAULT_PATH))
		.header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
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

#[cfg(test)]
#[path = "usage_report_tests.rs"]
mod tests;
