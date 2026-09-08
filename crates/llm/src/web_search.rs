//! Web-search server-tool detection (FR-2.6 request-side half).
//!
//! Mirrors `kong/llm/server_tools/init.lua`'s registry and `detect()` function.
//! Detection is registry-driven and matches on a request `tools[]` entry's
//! `type` field (not `name` — `name` is the function-tool discriminator on the
//! Anthropic and OpenAI Chat surfaces).
//!
//! This module lives in the `llm` crate (not `agentgateway`) because the
//! `RequestType` trait returns `WebSearchTrigger`s and the trait is defined
//! here. The policy *config* (`WebSearchConfig` — sidecar URL, streaming,
//! enabled-tools) lives in `agentgateway::llm::policy::web_search`, which is
//! the layer that decides whether to act on a detected trigger.
//!
//! The response-side marker bridge (the other half of FR-2.6) lives in
//! `crates/llm/src/conversion/web_search.rs`.

use serde::{Deserialize, Serialize};

/// Registered server-tool `type` names that map to the `web_search` tool.
///
/// Mirrors `server_tools.tool_types.web_search` in
/// `kong/llm/server_tools/init.lua:9-15`. A request `tools[]` entry whose
/// `type` matches one of these triggers the web-search sidecar. `web_search`
/// and `web_search_preview` are the OpenAI Responses spellings; the
/// `web_search_20250305` family is Anthropic.
pub const WEB_SEARCH_TYPES: &[&str] = &[
	"web_search_20250305",
	"web_search_20260209",
	"web_search_20260318",
	"web_search",
	"web_search_preview",
];

/// Returns `true` if a request `tools[]` entry's `type` is a registered
/// web-search server tool. Mirrors `server_tools.is_server_tool`.
pub fn is_web_search_server_tool(t: &serde_json::Value) -> bool {
	let Some(typ) = t.get("type").and_then(|v| v.as_str()) else {
		return false;
	};
	WEB_SEARCH_TYPES.contains(&typ)
}

/// A web-search server tool detected in a client request, plus the per-tool
/// options extracted from the client's tool object (forwarded to the sidecar
/// via `x-ai-ws-config`). Mirrors `server_tools.detect`'s `{name, opts}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchTrigger {
	/// The registered tool name (`"web_search"`); the sidecar dispatches on this.
	pub name: &'static str,
	/// Options extracted from the client's server-tool object. Forwarded to the
	/// sidecar inside `x-ai-ws-config.tools[]`. Mirrors
	/// `server_tools.extract_options.web_search` (init.lua:102-109).
	pub max_uses: Option<u64>,
	pub allowed_domains: Option<Vec<String>>,
	pub blocked_domains: Option<Vec<String>>,
	/// `user_location` is forwarded verbatim (city/region/country/timezone) —
	/// the sidecar reads `user_location.{city,region,country,timezone}`
	/// (`main.go:470-482`).
	pub user_location: Option<serde_json::Value>,
}

impl WebSearchTrigger {
	/// Extract a web-search trigger from a client `tools[]` entry whose `type`
	/// is a registered web-search server tool. Returns `None` for non-matching
	/// entries. Mirrors `server_tools.extract_options.web_search`.
	pub fn from_tool(t: &serde_json::Value) -> Option<Self> {
		if !is_web_search_server_tool(t) {
			return None;
		}
		Some(WebSearchTrigger {
			name: "web_search",
			max_uses: t.get("max_uses").and_then(|v| v.as_u64()),
			allowed_domains: t
				.get("allowed_domains")
				.and_then(|v| v.as_array())
				.map(|a| {
					a.iter()
						.filter_map(|v| v.as_str().map(String::from))
						.collect()
				}),
			blocked_domains: t
				.get("blocked_domains")
				.and_then(|v| v.as_array())
				.map(|a| {
					a.iter()
						.filter_map(|v| v.as_str().map(String::from))
						.collect()
				}),
			user_location: t.get("user_location").cloned(),
		})
	}
}

/// Scan a request's `tools[]` (raw JSON) for registered web-search server
/// tools. Mirrors `server_tools.detect(request_table.tools)`
/// (`kong/llm/server_tools/init.lua:374-385`): entries are matched on `type`,
/// and each registered hit yields one trigger. Returns every detected tool in
/// request order; the caller filters to those enabled on the target.
pub fn detect(tools: &[serde_json::Value]) -> Vec<WebSearchTrigger> {
	tools.iter().filter_map(WebSearchTrigger::from_tool).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn detects_anthropic_server_tool_by_type() {
		let tools = vec![json!({
			"type": "web_search_20250305",
			"max_uses": 5,
			"allowed_domains": ["example.com"],
			"blocked_domains": ["bad.example"],
			"user_location": {"city": "SF"}
		})];
		let found = detect(&tools);
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].name, "web_search");
		assert_eq!(found[0].max_uses, Some(5));
		assert_eq!(
			found[0].allowed_domains.as_deref(),
			Some(&["example.com".to_string()][..])
		);
		assert_eq!(found[0].user_location, Some(json!({"city": "SF"})));
	}

	#[test]
	fn detects_openai_responses_variant() {
		assert_eq!(detect(&[json!({"type": "web_search_preview"})]).len(), 1);
		assert_eq!(detect(&[json!({"type": "web_search"})]).len(), 1);
	}

	#[test]
	fn ignores_function_tool_named_web_search() {
		// A function tool with type "function" and name "web_search" must NOT
		// trigger — detection is on `type`, not `name`.
		let tools = vec![json!({"type": "function", "function": {"name": "web_search"}})];
		assert!(detect(&tools).is_empty());
	}

	#[test]
	fn ignores_non_string_type() {
		assert!(!is_web_search_server_tool(&json!({"type": 42})));
		assert!(!is_web_search_server_tool(&json!({"name": "web_search_20250305"})));
		assert!(!is_web_search_server_tool(&json!(42)));
	}

	#[test]
	fn detect_handles_empty_and_non_array() {
		assert!(detect(&[]).is_empty());
	}
}
