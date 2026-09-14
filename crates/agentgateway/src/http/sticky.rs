//! G6 sticky affinity policy.

use std::time::Duration;

use agent_core::prelude::Strng;

/// Sticky configures soft api-key→backend affinity for prompt-cache hit rate.
#[derive(Default, Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
	/// CEL expression resolving to the affinity key. Defaults to the request's
	/// api-key sha256.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub key: Option<Strng>,
	/// How long a pin survives after last use. Default 10m.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ttl: Option<Duration>,
}
