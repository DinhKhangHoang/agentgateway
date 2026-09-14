//! G7 capacity caps policy.

use std::time::Duration;

use crate::serdes::{apply, schema_de};

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

/// Local/config capacity policy (YAML deserialization). Mirrors `LocalHealthPolicy`.
#[derive(Default)]
#[apply(schema_de!)]
pub struct LocalCapacity {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub inflight_cap: Option<i32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tpm_per_minute: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none", with = "crate::serde_dur_option")]
	pub cooldown: Option<Duration>,
}

impl TryFrom<LocalCapacity> for Policy {
	type Error = anyhow::Error;
	fn try_from(v: LocalCapacity) -> Result<Self, Self::Error> {
		Ok(Self {
			inflight_cap: v.inflight_cap,
			tpm_per_minute: v.tpm_per_minute,
			cooldown: v.cooldown,
		})
	}
}
