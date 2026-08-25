mod body;

use std::num::NonZeroU8;
use std::sync::Arc;
use std::time::Duration;

pub use body::ReplayBody;

use crate::cel::Expression;
use crate::store::HasExpressions;
use crate::*;

#[apply(schema!)]
#[cfg_attr(feature = "schema", schemars(rename = "RetryPolicy"))]
pub struct Policy {
	/// Total number of attempts, including the original request.
	#[serde(default = "default_attempts")]
	pub attempts: NonZeroU8,
	/// Delay between retry attempts.
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		with = "serde_dur_option"
	)]
	#[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
	pub backoff: Option<Duration>,
	/// HTTP response status codes that should be retried.
	#[serde(serialize_with = "ser_display_iter", deserialize_with = "de_codes")]
	#[cfg_attr(feature = "schema", schemars(with = "Vec<std::num::NonZeroU16>"))]
	pub codes: Box<[http::StatusCode]>,
	/// Maximum number of request-body bytes buffered in memory for retry replay.
	/// A request whose body exceeds this cannot be retried, because the bytes needed
	/// to replay it were never kept. Omitting this, or setting it to `0`, applies the
	/// default of 64 KiB — `0` is the unset sentinel here just as it is on the xDS
	/// path, not a request to buffer nothing.
	///
	/// Raise this to match the listener's `maxBufferSize` when large request bodies
	/// must stay retriable — LLM chat traffic carrying conversation history routinely
	/// exceeds the default, and exceeding it disables retries for that request. The
	/// cap applies per in-flight request, so the memory it admits is this value times
	/// the number of retriable requests in flight.
	#[serde(
		default = "default_max_replay_bytes",
		deserialize_with = "de_max_replay_bytes"
	)]
	pub max_replay_bytes: usize,
	/// CEL expression evaluated against the request before any attempt; when `false`,
	/// retries are disabled (only the initial attempt is made), e.g. `request.method == "GET"`.
	/// Retrying requires buffering the request body in memory for replay, so this lets us skip
	/// that cost when the request is known to be non-retriable (e.g. streaming or websockets).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub precondition: Option<Arc<Expression>>,
	/// CEL expression evaluated against each response to decide whether to retry. A response
	/// is retried when its status code is in `codes` *or* this expression evaluates to `true`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub condition: Option<Arc<Expression>>,
}

impl HasExpressions for Policy {
	/// Exposes the precondition/condition expressions so the proxy snapshots the
	/// request/response attributes they reference.
	fn expressions(&self) -> impl Iterator<Item = &Expression> {
		self
			.precondition
			.iter()
			.chain(self.condition.iter())
			.map(|e| e.as_ref())
	}
}

pub fn de_codes<'de: 'a, 'a, D>(deserializer: D) -> Result<Box<[http::StatusCode]>, D::Error>
where
	D: Deserializer<'de>,
{
	let raw = Vec::<u16>::deserialize(deserializer)?;
	let boxed = raw
		.into_iter()
		.map(|c| http::StatusCode::from_u16(c).map_err(serde::de::Error::custom))
		.collect::<Result<Vec<_>, _>>()?;
	Ok(boxed.into_boxed_slice())
}
/// Maps an explicit `0` to the default so it means the same thing on the static config
/// path as it does on the xDS path, where 0 is the unset sentinel. Taken literally, `0`
/// would make `ReplayBody::try_new` reject every body with a non-zero size hint, turning
/// retries off for all but empty requests with nothing but a warning to show for it.
pub fn de_max_replay_bytes<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
	D: Deserializer<'de>,
{
	let raw = usize::deserialize(deserializer)?;
	Ok(if raw == 0 {
		default_max_replay_bytes()
	} else {
		raw
	})
}

fn default_attempts() -> NonZeroU8 {
	NonZeroU8::new(1).unwrap()
}
pub(crate) fn default_max_replay_bytes() -> usize {
	64 * 1024
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_pre_and_post_conditions() {
		let pol: Policy = serde_json::from_value(serde_json::json!({
			"attempts": 3,
			"codes": [503],
			"precondition": "request.method == \"GET\"",
			"condition": "response.headers[\"x-req-failed\"] != \"\"",
		}))
		.unwrap();
		assert_eq!(pol.attempts.get(), 3);
		assert_eq!(
			pol.precondition.as_ref().unwrap().original_expression,
			"request.method == \"GET\""
		);
		assert_eq!(
			pol.condition.as_ref().unwrap().original_expression,
			"response.headers[\"x-req-failed\"] != \"\""
		);
	}

	#[test]
	fn conditions_default_to_none() {
		let pol: Policy = serde_json::from_value(serde_json::json!({
			"attempts": 1,
			"codes": [500],
		}))
		.unwrap();
		assert!(pol.precondition.is_none());
		assert!(pol.condition.is_none());
	}

	#[test]
	fn policy_default_max_replay_bytes_is_64k() {
		let p: Policy = serde_json::from_str(r#"{"attempts":2,"codes":[503]}"#).expect("policy parses");
		assert_eq!(p.max_replay_bytes, 64 * 1024);
	}

	#[test]
	fn policy_explicit_zero_max_replay_bytes_is_the_default() {
		// 0 is the unset sentinel on the xDS path; the static path must agree, or a
		// literal 0 would disable retries for every request carrying a body.
		let p: Policy = serde_json::from_str(r#"{"attempts":2,"codes":[503],"maxReplayBytes":0}"#)
			.expect("policy parses");
		assert_eq!(p.max_replay_bytes, default_max_replay_bytes());
	}

	#[test]
	fn policy_max_replay_bytes_is_configurable() {
		let p: Policy =
			serde_json::from_str(r#"{"attempts":2,"codes":[503],"maxReplayBytes":52428800}"#)
				.expect("policy parses");
		assert_eq!(p.max_replay_bytes, 52_428_800);
	}

	#[test]
	fn expressions_exposes_both_conditions() {
		let pol: Policy = serde_json::from_value(serde_json::json!({
			"attempts": 2,
			"codes": [],
			"precondition": "request.method == \"GET\"",
			"condition": "response.code == 200",
		}))
		.unwrap();
		assert_eq!(pol.expressions().count(), 2);
	}
}
