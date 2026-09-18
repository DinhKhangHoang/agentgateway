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
	/// Use rendezvous consistent hashing instead of P2C for non-pinned requests.
	pub consistent_hash: bool,
}

impl<'a> SelectionContext<'a> {
	pub fn none() -> Self {
		Self { key: None, input_tokens: None, consistent_hash: false }
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

/// Rendezvous hashing (HRW — Highest Random Weight) for consistent-hash
/// endpoint selection. O(n) per selection, deterministic, and minimally
/// disruptive when endpoints are added/removed. Returns the endpoint name
/// that maximises `hash(key || endpoint_name)`.
pub fn consistent_select<'a, I>(key: &str, endpoints: I) -> Option<&'a str>
where
	I: IntoIterator<Item = &'a str>,
{
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};

	endpoints
		.into_iter()
		.max_by_key(|name| {
			let mut hasher = DefaultHasher::new();
			key.hash(&mut hasher);
			name.hash(&mut hasher);
			Hasher::finish(&hasher)
		})
		.map(|name| name)
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
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None, consistent_hash: false };
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
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None, consistent_hash: false };
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
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None, consistent_hash: false };
		assert!(!is_pinned("be-a", &ctx, Some(&state)));
	}

	// --- Integration tests (Task 9) ---

	#[test]
	fn composed_score_none_reduces_to_raw() {
		let info = EndpointInfo::default();
		let raw = info.score();
		let composed = composed_score("be-a", &info, &SelectionContext::none(), None, None);
		assert_eq!(composed, raw);
	}

	#[test]
	fn pinned_under_cap_beats_unpinned_under_cap() {
		use crate::store::PinEntry;
		use agent_core::prelude::Strng;
		use std::time::Duration;

		let state = SelectionState::default();
		state.pins.insert(
			Strng::from("key-a"),
			PinEntry {
				backend_name: Strng::from("ep-1"),
				expires_at: Instant::now() + Duration::from_secs(60),
			},
		);
		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None, consistent_hash: false };

		let info = EndpointInfo::default();
		let raw = info.score();

		let score_pinned = composed_score("ep-1", &info, &ctx, Some(&state), None);
		let score_unpinned = composed_score("ep-2", &info, &ctx, Some(&state), None);

		// Pinned (×2.0) should beat unpinned (×1.0) with equal raw scores.
		assert_eq!(score_pinned, raw * PINNED_UNDER_CAP);
		assert_eq!(score_unpinned, raw * UNPINNED_UNDER_CAP);
		assert!(score_pinned > score_unpinned);
	}

	#[test]
	fn tpm_over_cap_deprioritizes() {
		use agent_core::prelude::Strng;
		use std::time::Duration;

		let state = SelectionState::default();
		let now = Instant::now();
		let ctr = crate::store::TpmCounter::new(now);
		// Exhaust the TPM budget: 1000 debited, cap 1000.
		ctr.check_and_debit(1000, 1000, now);
		state.tpm.insert(Strng::from("ep-1"), ctr);

		let capacity = CapacityPolicy {
			inflight_cap: None,
			tpm_per_minute: Some(1000),
			cooldown: Some(Duration::from_secs(3)),
		};
		let ctx = SelectionContext { key: None, input_tokens: Some(500), consistent_hash: false };

		let info = EndpointInfo::default();
		let raw = info.score();

		// ep-1 is over TPM cap → ×0.5; ep-2 is fresh → ×1.0.
		let score_over = composed_score("ep-1", &info, &ctx, Some(&state), Some(&capacity));
		let score_fresh = composed_score("ep-2", &info, &ctx, Some(&state), Some(&capacity));

		assert_eq!(score_over, raw * UNPINNED_OVER_CAP);
		assert_eq!(score_fresh, raw * UNPINNED_UNDER_CAP);
		assert!(score_fresh > score_over);
	}

	#[test]
	fn per_pod_pin_divergence() {
		use crate::store::PinEntry;
		use agent_core::prelude::Strng;
		use std::time::Duration;

		// Two SelectionStates (simulating two pods). Same key pins to different backends.
		let state_a = SelectionState::default();
		let state_b = SelectionState::default();
		let ttl = Duration::from_secs(60);
		state_a.pins.insert(
			Strng::from("key-a"),
			PinEntry { backend_name: Strng::from("ep-1"), expires_at: Instant::now() + ttl },
		);
		state_b.pins.insert(
			Strng::from("key-a"),
			PinEntry { backend_name: Strng::from("ep-2"), expires_at: Instant::now() + ttl },
		);

		let ctx = SelectionContext { key: Some("key-a"), input_tokens: None, consistent_hash: false };
		assert!(is_pinned("ep-1", &ctx, Some(&state_a)));
		assert!(!is_pinned("ep-2", &ctx, Some(&state_a)));
		assert!(is_pinned("ep-2", &ctx, Some(&state_b)));
		assert!(!is_pinned("ep-1", &ctx, Some(&state_b)));
	}

	#[test]
	fn consistent_select_is_deterministic() {
		// Same key + same endpoint set → same result every time.
		let endpoints = vec!["ep-a", "ep-b", "ep-c"];
		let first = consistent_select("key-1", endpoints.iter().copied()).unwrap();
		let second = consistent_select("key-1", endpoints.iter().copied()).unwrap();
		assert_eq!(first, second, "HRW must be deterministic for the same key");
	}

	#[test]
	fn consistent_select_different_keys_can_differ() {
		// With enough keys and endpoints, not all keys should map to the same endpoint.
		let endpoints = vec!["ep-a", "ep-b", "ep-c"];
		let results: std::collections::HashSet<_> = (0..30)
			.map(|i| consistent_select(&format!("key-{i}"), endpoints.iter().copied()).unwrap())
			.collect();
		assert!(
			results.len() > 1,
			"HRW should distribute across endpoints, not always pick the same one"
		);
	}

	#[test]
	fn consistent_select_empty_returns_none() {
		let result = consistent_select("key-1", std::iter::empty());
		assert!(result.is_none());
	}
}
