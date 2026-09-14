//! G7 capacity caps policy.

use std::time::Duration;

/// Capacity configures per-endpoint inflight + TPM hard caps.
/// Reject 503+Retry-After on trip.
#[derive(Default, Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
	/// Max concurrent in-flight requests per endpoint.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub inflight_cap: Option<i32>,
	/// Per-endpoint tokens-per-minute budget. Pre-debited at select,
	/// trued-up on completion.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tpm_per_minute: Option<i64>,
	/// Hold-rejected duration. Default 3s (reuses Retry-After).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cooldown: Option<Duration>,
}
