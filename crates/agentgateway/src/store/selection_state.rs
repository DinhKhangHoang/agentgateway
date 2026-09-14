//! G6/G7 per-pod selection state: sticky pins + per-endpoint TPM counters.
//!
//! Hung off `Store` (next to `ProberGenerationRegistry`). Per-pod only —
//! see the design spec's stated limitations.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_core::prelude::Strng;
use dashmap::DashMap;

/// G6: api-key sha256 → pinned backend, TTL'd.
#[derive(Clone, Debug)]
pub struct PinEntry {
	pub backend_name: Strng,
	pub expires_at: Instant,
}

impl PinEntry {
	pub fn is_expired(&self, now: Instant) -> bool {
		now >= self.expires_at
	}
}

/// G7: per-endpoint TPM fixed-window counter (60s window).
///
/// All fields are atomic for lock-free access through `DashMap`'s `&TpmCounter`.
/// `window_start_nanos` stores nanos since `Instant::now()` at construction;
/// window rollover is detected by elapsed time ≥ 60s.
#[derive(Debug)]
pub struct TpmCounter {
	window_start_nanos: AtomicU64,
	debited: AtomicU64,
	actual: AtomicU64,
}

impl TpmCounter {
	const WINDOW: Duration = Duration::from_secs(60);

	pub fn new(now: Instant) -> Self {
		Self {
			window_start_nanos: AtomicU64::new(now.elapsed().as_nanos() as u64),
			debited: AtomicU64::new(0),
			actual: AtomicU64::new(0),
		}
	}

	/// Returns (total_after_debit, is_over_cap). Rolls window if stale.
	/// On over-cap, does NOT debit (reject without consuming budget).
	pub fn check_and_debit(&self, input_tokens: u64, cap: u64, now: Instant) -> (u64, bool) {
		let now_nanos = now.elapsed().as_nanos() as u64;
		let start = self.window_start_nanos.load(Ordering::Relaxed);
		if now_nanos.saturating_sub(start) >= Self::WINDOW.as_nanos() as u64 {
			// Window rollover: reset both counters.
			self.debited.store(0, Ordering::Relaxed);
			self.actual.store(0, Ordering::Relaxed);
			self.window_start_nanos.store(now_nanos, Ordering::Relaxed);
		}
		let cur = self.debited.load(Ordering::Relaxed);
		let total = cur.saturating_add(input_tokens);
		let over = total > cap;
		if !over {
			self.debited.fetch_add(input_tokens, Ordering::Relaxed);
		}
		(total, over)
	}

	/// Read-only check: returns true if over cap (no debit). Rolls window if stale.
	pub fn check(&self, input_tokens: u64, cap: u64, now: Instant) -> bool {
		let now_nanos = now.elapsed().as_nanos() as u64;
		let start = self.window_start_nanos.load(Ordering::Relaxed);
		if now_nanos.saturating_sub(start) >= Self::WINDOW.as_nanos() as u64 {
			return false; // window rolled over, fresh budget
		}
		let cur = self.debited.load(Ordering::Relaxed);
		cur.saturating_add(input_tokens) > cap
	}

	/// True-up on `finish_request`: replace the pre-debit with actual usage.
	pub fn trued_up(&self, pre_debited: u64, actual_tokens: u64) {
		self.actual.fetch_add(actual_tokens, Ordering::Relaxed);
		// Release the pre-debit, keep actual.
		let cur = self.debited.load(Ordering::Relaxed);
		self
			.debited
			.fetch_sub(pre_debited.min(cur), Ordering::Relaxed);
	}
}

/// Per-pod selection state, hung off `Store` (next to `ProberGenerationRegistry`).
#[derive(Debug, Default)]
pub struct SelectionState {
	pub pins: DashMap<Strng, PinEntry>,
	pub tpm: DashMap<Strng, TpmCounter>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pin_entry_expiry() {
		let now = Instant::now();
		let entry = PinEntry {
			backend_name: Strng::from("be-a"),
			expires_at: now,
		};
		assert!(entry.is_expired(now));
		let future = PinEntry {
			backend_name: Strng::from("be-a"),
			expires_at: now + Duration::from_secs(60),
		};
		assert!(!future.is_expired(now));
	}

	#[test]
	fn tpm_under_cap_debits() {
		let now = Instant::now();
		let ctr = TpmCounter::new(now);
		let (total, over) = ctr.check_and_debit(500, 1000, now);
		assert_eq!(total, 500);
		assert!(!over);
		assert_eq!(ctr.debited.load(Ordering::Relaxed), 500);
	}

	#[test]
	fn tpm_over_cap_rejects_without_debit() {
		let now = Instant::now();
		let ctr = TpmCounter::new(now);
		ctr.check_and_debit(800, 1000, now);
		let (total, over) = ctr.check_and_debit(300, 1000, now);
		assert!(over);
		assert_eq!(total, 1100);
		// rejected: not debited
		assert_eq!(ctr.debited.load(Ordering::Relaxed), 800);
	}

	#[test]
	fn tpm_true_up_frees_phantom_debt() {
		let now = Instant::now();
		let ctr = TpmCounter::new(now);
		ctr.check_and_debit(1000, 1000, now); // debited=1000
		ctr.trued_up(1000, 200); // actual=200, release 1000 pre-debit
		assert_eq!(ctr.actual.load(Ordering::Relaxed), 200);
		assert_eq!(ctr.debited.load(Ordering::Relaxed), 0);
		// 800 tokens freed back to the window
		let (total, over) = ctr.check_and_debit(800, 1000, now);
		assert!(!over);
		assert_eq!(total, 800);
	}
}
