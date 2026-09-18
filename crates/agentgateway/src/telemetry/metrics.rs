use std::fmt::Debug;

use agent_core::metrics::{CustomField, DefaultedUnknown, EncodeArc, EncodeDebug, EncodeDisplay};
use agent_core::strng::RichStrng;
use agent_core::version;
use frozen_collections::FzHashSet;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram as PromHistogram;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::{Metric, Registry, Unit};
use tracing::{debug, trace};

use crate::mcp::MCPOperation;
use crate::proxy::ProxyResponseReason;
use crate::types::agent::TransportProtocol;

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct RouteIdentifier {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub route: DefaultedUnknown<RichStrng>,
	pub route_rule: DefaultedUnknown<RichStrng>,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum GuardrailPhase {
	#[default]
	Request,
	Response,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum GuardrailAction {
	#[default]
	Allow,
	Mask,
	Reject,
	FailOpen,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GuardrailLabels {
	pub phase: GuardrailPhase,
	pub action: GuardrailAction,
}

// --- WS-5 Serving-SRE telemetry label types ---

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum BalanceStrategy {
	#[default]
	P2c,
	ConsistentHash,
	Fallback,
	Degraded,
	Override,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum ProbeOutcome {
	Success,
	#[default]
	Failure,
	Skipped,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum EvictionReason {
	#[default]
	ConsecutiveFailures,
	HealthThreshold,
	RequestFailure,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum EvictionSource {
	#[default]
	Probe,
	Request,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct BalanceLabels {
	pub backend: DefaultedUnknown<RichStrng>,
	pub strategy: BalanceStrategy,
	#[prometheus(flatten)]
	pub route: RouteIdentifier,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct HealthProbeLabels {
	pub backend: DefaultedUnknown<RichStrng>,
	pub outcome: ProbeOutcome,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct HealthEvictionLabels {
	pub backend: DefaultedUnknown<RichStrng>,
	pub reason: EvictionReason,
	pub source: EvictionSource,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MinimalHTTPLabels {
	pub backend: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

impl From<HTTPLabels> for MinimalHTTPLabels {
	fn from(value: HTTPLabels) -> Self {
		Self {
			backend: value.backend,
			route: value.route,
			custom: value.custom,
		}
	}
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct HTTPLabels {
	pub backend: DefaultedUnknown<RichStrng>,
	pub protocol: DefaultedUnknown<EncodeDebug<crate::cel::BackendProtocol>>,

	pub method: DefaultedUnknown<EncodeDisplay<http::Method>>,
	pub status: DefaultedUnknown<EncodeDisplay<u16>>,
	pub reason: DefaultedUnknown<EncodeDisplay<ProxyResponseReason>>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabels {
	pub gen_ai_operation_name: DefaultedUnknown<RichStrng>,
	pub gen_ai_system: DefaultedUnknown<RichStrng>,
	pub gen_ai_request_model: DefaultedUnknown<RichStrng>,
	pub gen_ai_response_model: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabelsTokenUsage {
	pub gen_ai_token_type: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub common: EncodeArc<GenAILabels>,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct CostCatalogLookupLabels {
	pub status: crate::llm::cost::CostLookupStatus,

	#[prometheus(flatten)]
	pub common: EncodeArc<GenAILabels>,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MCPCall {
	pub method: DefaultedUnknown<RichStrng>,

	pub resource_type: DefaultedUnknown<MCPOperation>,
	pub server: DefaultedUnknown<RichStrng>,
	pub resource: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct TCPLabels {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub protocol: TransportProtocol,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct ConnectLabels {
	pub transport: DefaultedUnknown<RichStrng>,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum OutboundCallKind {
	/// The primary backend call
	#[default]
	Primary,
	/// A callout as part of a policy execution
	Policy,
	/// A mirrored call
	Mirror,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
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

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct OutboundCallLabels {
	pub kind: OutboundCallKind,
	pub subtype: OutboundCallSubtype,
}

type Counter = Family<HTTPLabels, counter::Counter>;
type Histogram<T> = Family<T, prometheus_client::metrics::histogram::Histogram>;
type TCPCounter = Family<TCPLabels, counter::Counter>;

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildLabel {
	tag: &'static str,
}

#[derive(Debug)]
pub struct Metrics {
	pub requests: Counter,
	pub request_duration: Histogram<HTTPLabels>,
	pub request_processing_duration: Histogram<MinimalHTTPLabels>,
	pub response_processing_duration: Histogram<MinimalHTTPLabels>,
	pub response_bytes: Family<HTTPLabels, counter::Counter>,

	pub mcp_requests: Family<MCPCall, counter::Counter>,

	pub gen_ai_token_usage: Histogram<GenAILabelsTokenUsage>,
	pub gen_ai_cost: Family<GenAILabels, counter::Counter<f64>>,
	pub gen_ai_request_duration: Histogram<GenAILabels>,
	pub gen_ai_time_per_output_token: Histogram<GenAILabels>,
	pub gen_ai_time_to_first_token: Histogram<GenAILabels>,

	pub tls_handshake_duration: Histogram<TCPLabels>,

	pub downstream_connection: TCPCounter,
	pub tcp_downstream_rx_bytes: Family<TCPLabels, counter::Counter>,
	pub tcp_downstream_tx_bytes: Family<TCPLabels, counter::Counter>,

	pub upstream_connect_duration: Histogram<ConnectLabels>,
	pub upstream_call_duration: Histogram<OutboundCallLabels>,

	// metrics for guardrail checks (allow/mask/reject) for request/response
	pub guardrail_checks: Family<GuardrailLabels, counter::Counter>,

	// WS-5: Serving-SRE telemetry — balance selection, health probing, eviction.
	pub balance_picks: Family<BalanceLabels, counter::Counter>,
	pub balance_exhausted: Family<BalanceLabels, counter::Counter>,
	pub tpm_reserved: Family<BalanceLabels, counter::Counter>,
	pub tpm_trueup_delta: Family<BalanceLabels, counter::Counter>,
	pub health_probe: Family<HealthProbeLabels, counter::Counter>,
	pub health_eviction: Family<HealthEvictionLabels, counter::Counter>,

	pub cost_catalog_lookups: Family<CostCatalogLookupLabels, counter::Counter>,

	/// Usage reports abandoned after the final retry, or shed because the
	/// in-flight cap was reached. Non-zero means unbilled usage: alert on it.
	pub llm_usage_report_dropped: counter::Counter,

	// metrics for request retries
	pub retries: Counter,
}

// FilteredRegistry is a wrapper around Registry that allows to filter out certain metrics.
// Note: this currently only excludes them from the registry, but the underlying metrics are still
// stored. This can result in memory cost, etc to store the labels.
// A more robust future solution would be to have a sort of `Disabled` metric that does not store;
// note that even still, we would be computing the labels (and then dropping them), but in many cases
// the same labels are shared by many metrics, and are cheap to construct, so likely not a major concern.
struct FilteredRegistry<'a> {
	registry: &'a mut Registry,
	removes: FzHashSet<String>,
}

impl<'a> FilteredRegistry<'a> {
	fn should_skip(&self, name: &str, unit: Option<&Unit>) -> bool {
		let mut names = vec![
			name.to_string(),
			format!("{}_total", name),
			format!("{}_{}_total", agent_core::metrics::PREFIX, name),
			format!("{}_{}", agent_core::metrics::PREFIX, name),
		];
		if let Some(unit) = unit {
			names.extend_from_slice(&[
				format!("{}_{}", name, unit.as_str()),
				format!("{}_{}_total", name, unit.as_str()),
				format!(
					"{}_{}_{}_total",
					agent_core::metrics::PREFIX,
					name,
					unit.as_str()
				),
				format!("{}_{}_{}", agent_core::metrics::PREFIX, name, unit.as_str()),
			])
		}

		for n in names.into_iter() {
			let exclude = self.removes.contains(&n);
			trace!(name = n, exclude, "check metric for exclusion");
			if exclude {
				return true;
			}
		}
		false
	}
	fn register(&mut self, name: impl Into<String>, help: impl Into<String>, metric: impl Metric) {
		let name = name.into();
		if self.should_skip(&name, None) {
			debug!("skip register metric: {}", name);
			return;
		}
		self.registry.register(name, help, metric);
	}

	fn register_with_unit(
		&mut self,
		name: impl Into<String>,
		help: impl Into<String>,
		unit: Unit,
		metric: impl Metric,
	) {
		let name = name.into();
		if self.should_skip(&name, Some(&unit)) {
			debug!("skip register metric: {}_{}", name, unit.as_str());
			return;
		}
		self.registry.register_with_unit(name, help, unit, metric);
	}
}

impl Metrics {
	pub fn new(registry: &mut Registry, removes: FzHashSet<String>) -> Self {
		let mut registry = FilteredRegistry { registry, removes };
		registry.register(
			"build",
			"Agentgateway build information",
			Info::new(BuildLabel {
				tag: version::BuildInfo::new().version,
			}),
		);

		let gen_ai_token_usage = Family::<GenAILabelsTokenUsage, _>::new_with_constructor(move || {
			PromHistogram::new(TOKEN_USAGE_BUCKET)
		});
		registry.register(
			"gen_ai_client_token_usage",
			"Number of tokens used per request",
			gen_ai_token_usage.clone(),
		);

		let gen_ai_cost = Family::<GenAILabels, _>::default();
		registry.register_with_unit(
			"gen_ai_client_cost",
			"Cumulative USD cost of generative AI requests",
			Unit::Other("usd".to_string()),
			gen_ai_cost.clone(),
		);

		// TODO: add error attribute if it ends with an error
		let gen_ai_request_duration = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(REQUEST_DURATION_BUCKET)
		});
		registry.register(
			"gen_ai_server_request_duration",
			"Duration of generative AI request",
			gen_ai_request_duration.clone(),
		);

		let gen_ai_time_per_output_token = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(OUTPUT_TOKEN_BUCKET)
		});
		registry.register(
			"gen_ai_server_time_per_output_token",
			"Time to generate each output token for a given request",
			gen_ai_time_per_output_token.clone(),
		);

		let gen_ai_time_to_first_token = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(FIRST_TOKEN_BUCKET)
		});
		registry.register(
			"gen_ai_server_time_to_first_token",
			"Time to generate the first token for a given request",
			gen_ai_time_to_first_token.clone(),
		);

		Metrics {
			requests: build(
				&mut registry,
				"requests",
				"The total number of HTTP requests sent",
			),
			guardrail_checks: {
				let m = Family::<GuardrailLabels, _>::default();
				registry.register(
					"guardrail_checks",
					"Total number of guardrail checks",
					m.clone(),
				);
				m
			},
			balance_picks: {
				let m = Family::<BalanceLabels, _>::default();
				registry.register(
					"balance_picks_total",
					"Total endpoint/provider selections by strategy",
					m.clone(),
				);
				m
			},
			balance_exhausted: {
				let m = Family::<BalanceLabels, _>::default();
				registry.register(
					"balance_exhausted_total",
					"Total selection exhaustions (no healthy endpoint, yields NoHealthyEndpoints 503)",
					m.clone(),
				);
				m
			},
			tpm_reserved: {
				let m = Family::<BalanceLabels, _>::default();
				registry.register(
					"tpm_reserved_tokens_total",
					"Total tokens pre-debited against TPM limits",
					m.clone(),
				);
				m
			},
			tpm_trueup_delta: {
				let m = Family::<BalanceLabels, _>::default();
				registry.register(
					"tpm_trueup_delta_tokens_total",
					"Token delta between pre-debit estimate and actual usage (true-up)",
					m.clone(),
				);
				m
			},
			health_probe: {
				let m = Family::<HealthProbeLabels, _>::default();
				registry.register(
					"health_probe_total",
					"Total active health probe attempts by outcome",
					m.clone(),
				);
				m
			},
			health_eviction: {
				let m = Family::<HealthEvictionLabels, _>::default();
				registry.register(
					"health_eviction_total",
					"Total endpoint evictions by reason and source",
					m.clone(),
				);
				m
			},
			cost_catalog_lookups: {
				let m = Family::<CostCatalogLookupLabels, _>::default();
				registry.register(
					"cost_catalog_lookups",
					"Total number of model cost catalog lookups by resolution status",
					m.clone(),
				);
				m
			},
			llm_usage_report_dropped: {
				let m = counter::Counter::default();
				registry.register(
					"llm_usage_report_dropped",
					"Total number of LLM usage reports that were never delivered; each one is unbilled usage",
					m.clone(),
				);
				m
			},
			downstream_connection: build(
				&mut registry,
				"downstream_connections",
				"The total number of downstream connections established",
			),

			mcp_requests: build(
				&mut registry,
				"mcp_requests",
				"Total number of MCP tool calls",
			),

			gen_ai_token_usage,
			gen_ai_cost,
			gen_ai_request_duration,
			gen_ai_time_per_output_token,
			gen_ai_time_to_first_token,

			response_bytes: {
				let m = Family::<HTTPLabels, _>::default();
				registry.register_with_unit(
					"response",
					"Total HTTP response bytes received",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			request_duration: {
				let m = Family::<HTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(HTTP_REQUEST_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"request_duration",
					"Duration of HTTP requests (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			request_processing_duration: {
				let m = Family::<MinimalHTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(PROCESSING_DURATION_BUCKETS)
				});
				registry.register_with_unit(
					"request_processing",
					"Duration from receiving an HTTP request to sending the primary outbound call (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			response_processing_duration: {
				let m = Family::<MinimalHTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(PROCESSING_DURATION_BUCKETS)
				});
				registry.register_with_unit(
					"response_processing",
					"Duration from receiving the primary outbound response to sending the HTTP response (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			tcp_downstream_rx_bytes: {
				let m = Family::<TCPLabels, _>::default();
				registry.register_with_unit(
					"downstream_received",
					"Total TCP bytes received per connection labels",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			tcp_downstream_tx_bytes: {
				let m = Family::<TCPLabels, _>::default();
				registry.register_with_unit(
					"downstream_sent",
					"Total TCP bytes transmitted per connection labels",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			upstream_connect_duration: {
				let m = Family::<ConnectLabels, _>::new_with_constructor(move || {
					PromHistogram::new(CONNECT_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"upstream_connect_duration",
					"Duration to establish upstream connection (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			upstream_call_duration: {
				let m = Family::<OutboundCallLabels, _>::new_with_constructor(move || {
					PromHistogram::new(HTTP_REQUEST_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"upstream_call_duration",
					"Duration of outbound calls made by agentgateway (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			tls_handshake_duration: {
				let m = Family::<TCPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(CONNECT_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"tls_handshake_duration",
					"Duration to complete inbound TLS/HTTPS handshake (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			retries: build(
				&mut registry,
				"retries",
				"The total number of request retries",
			),
		}
	}
}

fn build<'a, T: Clone + std::hash::Hash + Eq + Send + Sync + Debug + EncodeLabelSet + 'static>(
	registry: &mut FilteredRegistry<'a>,
	name: &str,
	help: &str,
) -> Family<T, counter::Counter> {
	let m = Family::<T, _>::default();
	registry.register(name, help, m.clone());
	m
}

// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiclienttokenusage
const TOKEN_USAGE_BUCKET: [f64; 14] = [
	1., 4., 16., 64., 256., 1024., 4096., 16384., 65536., 262144., 1048576., 4194304., 16777216.,
	67108864.,
];
// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiserverrequestduration
const REQUEST_DURATION_BUCKET: [f64; 14] = [
	0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
// Finer-grained, exponentially growing buckets for TCP/TLS connect.
// Keep in seconds (Prometheus convention). Prioritize sub-second resolution, with a few larger outlier buckets.
const CONNECT_DURATION_BUCKET: [f64; 10] = [
	0.0005, // 0.5 ms
	0.0015, // 1.5 ms
	0.0043, // 4.3 ms
	0.0126, // 12.6 ms
	0.0368, // 36.8 ms
	0.108,  // 108 ms
	0.316,  // 316 ms
	0.924,  // 924 ms
	2.71,   // 2.71 s
	8.0,    // 8 s
];
// HTTP request duration buckets - general purpose for all HTTP traffic
// Covers 1ms to ~80 seconds with exponential growth
const HTTP_REQUEST_DURATION_BUCKET: [f64; 14] = [
	0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 80.0,
];
// Internal processing time
// Covers 50us to 250ms with growth.
const PROCESSING_DURATION_BUCKETS: [f64; 10] = [
	0.00005, // 50us
	0.0001,  // 100us
	0.00025, // 250us
	0.0005,  // 500us
	0.001,   // 1ms
	0.0025,  // 2.5ms
	0.005,   // 5ms
	0.01,    // 10ms
	0.05,    // 50ms
	0.25,    // 250ms
];

// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiservertime_per_output_token
// NOTE: the spec has SHOULD, but is not smart enough to handle the faster LLMs.
// We have added 0.001 (1000 TPS)
const OUTPUT_TOKEN_BUCKET: [f64; 14] = [
	0.001, 0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0, 2.5,
];
// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiservertime_to_first_token
const FIRST_TOKEN_BUCKET: [f64; 16] = [
	0.001, 0.005, 0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

#[cfg(test)]
mod tests {
	use super::*;
	use prometheus_client::encoding::text::encode;

	fn fresh_metrics() -> (Registry, Metrics) {
		let mut registry = Registry::default();
		let metrics = Metrics::new(&mut registry, FzHashSet::default());
		(registry, metrics)
	}

	fn encode_registry(registry: &Registry) -> String {
		let mut buf = String::new();
		encode(&mut buf, registry).unwrap();
		buf
	}

	#[test]
	fn balance_picks_total_registered_and_incremented() {
		let (registry, metrics) = fresh_metrics();
		let labels = BalanceLabels {
			backend: Some(RichStrng::from("test-backend")).into(),
			strategy: BalanceStrategy::P2c,
			route: RouteIdentifier::default(),
		};
		metrics.balance_picks.get_or_create(&labels).inc();
		let output = encode_registry(&registry);
		assert!(
			output.contains("balance_picks_total"),
			"balance_picks_total should be in the encoded output"
		);
	}

	#[test]
	fn balance_exhausted_total_registered_and_incremented() {
		let (registry, metrics) = fresh_metrics();
		let labels = BalanceLabels::default();
		metrics.balance_exhausted.get_or_create(&labels).inc();
		let output = encode_registry(&registry);
		assert!(
			output.contains("balance_exhausted_total"),
			"balance_exhausted_total should be in the encoded output"
		);
	}

	#[test]
	fn health_probe_total_registered_and_incremented() {
		let (registry, metrics) = fresh_metrics();
		let labels = HealthProbeLabels {
			backend: Some(RichStrng::from("probe-target")).into(),
			outcome: ProbeOutcome::Success,
		};
		metrics.health_probe.get_or_create(&labels).inc();
		let output = encode_registry(&registry);
		assert!(
			output.contains("health_probe_total"),
			"health_probe_total should be in the encoded output"
		);
	}

	#[test]
	fn health_eviction_total_registered_and_incremented() {
		let (registry, metrics) = fresh_metrics();
		let labels = HealthEvictionLabels {
			backend: Some(RichStrng::from("evicted-backend")).into(),
			reason: EvictionReason::ConsecutiveFailures,
			source: EvictionSource::Probe,
		};
		metrics.health_eviction.get_or_create(&labels).inc();
		let output = encode_registry(&registry);
		assert!(
			output.contains("health_eviction_total"),
			"health_eviction_total should be in the encoded output"
		);
	}

	#[test]
	fn excluded_metrics_filter_hides_ws5_metrics() {
		let mut registry = Registry::default();
		let removes = FzHashSet::from_iter([
			"balance_picks_total".to_string(),
			"balance_exhausted_total".to_string(),
			"health_probe_total".to_string(),
			"health_eviction_total".to_string(),
		]);
		let _metrics = Metrics::new(&mut registry, removes);
		let mut buf = String::new();
		encode(&mut buf, &registry).unwrap();
		assert!(
			!buf.contains("balance_picks_total"),
			"excluded metric should not appear in registry"
		);
		assert!(
			!buf.contains("health_eviction_total"),
			"excluded metric should not appear in registry"
		);
	}
}
