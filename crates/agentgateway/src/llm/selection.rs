//! G6/G7 selection seam: `SelectionContext` + `composed_score`.
//!
//! Threaded through the AI-path endpoint selection chain so that G6 (sticky
//! affinity) and G7 (capacity caps) can bias/gate the P2C sampler without
//! changing the selection function's signature again. When no sticky/capacity
//! policy is configured, `SelectionContext` fields are `None` and
//! `composed_score` reduces to `EndpointInfo::score()` — today's behavior.

use std::time::Instant;

use crate::http::capacity::Policy as CapacityPolicy;
use crate::store::SelectionState;
use crate::types::loadbalancer::EndpointInfo;

/// Caller identity + request size, threaded through endpoint selection.
/// `None` fields => no sticky/capacity policy; selection is today's behavior.
#[derive(Debug, Clone, Copy)]
pub struct SelectionContext<'a> {
	/// api-key sha256, for G6 sticky pin lookup. None when no Sticky policy.
	pub key: Option<&'a str>,
	/// LLMRequest.input_tokens, for G7 TPM pre-debit. None when no Capacity policy.
	pub input_tokens: Option<u32>,
}

impl<'a> SelectionContext<'a> {
	pub fn none() -> Self {
		Self { key: None, input_tokens: None }
	}
}

// --- Bias multipliers (spec §G6 §2-3, §G7 §3) ---

/// Pinned endpoint that is under capacity cap: strong preference.
const PINNED_UNDER_CAP: f64 = 2.0;
/// Pinned endpoint over capacity cap: soft preference (loses to unpinned under-cap).
const PINNED_OVER_CAP: f64 = 1.2;
/// Unpinned endpoint under capacity cap: neutral.
const UNPINNED_UNDER_CAP: f64 = 1.0;
/// Unpinned endpoint over capacity cap: deprioritize.
const UNPINNED_OVER_CAP: f64 = 0.5;

/// Returns true if the endpoint name matches the pinned backend for `ctx.key`.
fn is_pinned(
	endpoint_name: &str,
	ctx: &SelectionContext<'_>,
	selection_state: Option<&SelectionState>,
) -> bool {
	let Some(key) = ctx.key else { return false; };
	let Some(state) = selection_state else { return false; };
	if let Some(entry) = state.pins.get(key) {
		if entry.is_expired(Instant::now()) {
			return false;
		}
		return entry.backend_name.as_str() == endpoint_name;
	}
	false
}

/// Read-only capacity check (no TPM debit). Returns true if the candidate is
/// over its inflight or TPM cap.
pub fn is_over_capacity(
	endpoint_name: &str,
	info: &EndpointInfo,
	ctx: &SelectionContext<'_>,
	capacity: Option<&CapacityPolicy>,
	selection_state: Option<&SelectionState>,
) -> bool {
	let Some(cap) = capacity else { return false; };
	// Inflight check (side-effect-free).
	let inflight_over = cap
		.inflight_cap
		.is_some_and(|c| info.pending_requests_count() as i32 >= c);
	// TPM check (read-only, no debit).
	let tpm_over = if let (Some(tpm_cap), Some(state), Some(tokens)) =
		(cap.tpm_per_minute, selection_state, ctx.input_tokens)
	{
		if let Some(entry) = state.tpm.get(endpoint_name) {
			entry.check(tokens as u64, tpm_cap as u64, Instant::now())
		} else {
			false // no counter yet → fresh budget
		}
	} else {
		false
	};
	inflight_over || tpm_over
}

/// Composed selection score applying G6 sticky bias + G7 capacity gate.
/// When `selection_state`/`capacity` are None, reduces to raw `score()`.
pub fn composed_score(
	endpoint_name: &str,
	info: &EndpointInfo,
	ctx: &SelectionContext<'_>,
	selection_state: Option<&SelectionState>,
	capacity: Option<&CapacityPolicy>,
) -> f64 {
	let raw = info.score();
	let pinned = is_pinned(endpoint_name, ctx, selection_state);
	let over_cap = is_over_capacity(endpoint_name, info, ctx, capacity, selection_state);
	let multiplier = match (pinned, over_cap) {
		(true, false) => PINNED_UNDER_CAP,
		(true, true) => PINNED_OVER_CAP,
		(false, false) => UNPINNED_UNDER_CAP,
		(false, true) => UNPINNED_OVER_CAP,
	};
	raw * multiplier
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn none_ctx_is_pass_through() {
		let ctx = SelectionContext::none();
		assert!(ctx.key.is_none());
		assert!(ctx.input_tokens.is_none());
	}

	#[test]
	fn bias_multipliers_ordered() {
		assert!(PINNED_UNDER_CAP > PINNED_OVER_CAP);
		assert!(PINNED_OVER_CAP > UNPINNED_UNDER_CAP);
		assert!(UNPINNED_UNDER_CAP > UNPINNED_OVER_CAP);
	}

	#[test]
	fn is_pinned_returns_false_without_state() {
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None };
		assert!(!is_pinned("be-a", &ctx, None));
	}

	#[test]
	fn is_pinned_returns_false_without_key() {
		let state = SelectionState::default();
		let ctx = SelectionContext::none();
		assert!(!is_pinned("be-a", &ctx, Some(&state)));
	}

	#[test]
	fn is_pinned_matches_live_pin() {
		use crate::store::PinEntry;
		use agent_core::prelude::Strng;
		use std::time::Duration;

		let state = SelectionState::default();
		state.pins.insert(
			Strng::from("key-a"),
			PinEntry {
				backend_name: Strng::from("be-a"),
				expires_at: Instant::now() + Duration::from_secs(60),
			},
		);
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None };
		assert!(is_pinned("be-a", &ctx, Some(&state)));
		assert!(!is_pinned("be-b", &ctx, Some(&state)));
	}

	#[test]
	fn is_pinned_ignores_expired_pin() {
		use crate::store::PinEntry;
		use agent_core::prelude::Strng;

		let state = SelectionState::default();
		state.pins.insert(
			Strng::from("key-a"),
			PinEntry {
				backend_name: Strng::from("be-a"),
				expires_at: Instant::now(),
			},
		);
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None };
		assert!(!is_pinned("be-a", &ctx, Some(&state)));
	}
}
