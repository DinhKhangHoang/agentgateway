//! G6/G7 selection seam: `SelectionContext` + `composed_score`.
//!
//! Threaded through the AI-path endpoint selection chain so that G6 (sticky
//! affinity) and G7 (capacity caps) can bias/gate the P2C sampler without
//! changing the selection function's signature again. When no sticky/capacity
//! policy is configured, `SelectionContext` fields are `None` and
//! `composed_score` reduces to `EndpointInfo::score()` — today's behavior.

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

/// Composed selection score. Currently pass-through to `EndpointInfo::score()`.
/// Tasks 4/6 extend this with sticky bias + capacity gate.
pub fn composed_score(info: &EndpointInfo, _ctx: &SelectionContext<'_>) -> f64 {
	info.score()
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
}
