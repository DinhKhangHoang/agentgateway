//! G6 sticky affinity policy.

use std::time::Duration;

use agent_core::prelude::Strng;

use crate::serdes::{apply, schema_de};

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

/// Local/config sticky policy (YAML deserialization). Mirrors `LocalHealthPolicy`.
#[derive(Default)]
#[apply(schema_de!)]
pub struct LocalSticky {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub key: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", with = "crate::serde_dur_option")]
	pub ttl: Option<Duration>,
}

impl TryFrom<LocalSticky> for Policy {
	type Error = anyhow::Error;
	fn try_from(v: LocalSticky) -> Result<Self, Self::Error> {
		Ok(Self {
			key: v.key.map(Strng::from),
			ttl: v.ttl,
		})
	}
}
