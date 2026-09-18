//! Web-search augmentation *config* (FR-2.6): the policy fields that decide
//! whether to act on a detected server tool. The detection itself (registry,
//! `WebSearchTrigger`, `detect`) lives in the `llm` crate at
//! `llm::web_search`, because the `RequestType` trait returns triggers and
//! that trait is defined in `llm` (the `llm` crate cannot depend on
//! `agentgateway`).
//!
//! This module mirrors the effective fields of Kong's
//! `model.server_tools_config` / legacy `model.web_search` block. The
//! response-side marker bridge lives in `crates/llm/src/conversion/web_search.rs`.

// The detection primitives (WebSearchTrigger, detect, ...) live in the extern
// `agent-llm` crate — `RequestType` (which returns triggers) is defined there,
// and `llm` cannot depend on `agentgateway`. Here in `agentgateway`,
// `crate::llm` is our own `llm/` module, so the extern crate is spelled
// `agent_llm` (hyphens → underscores), matching `pub use agent_llm::tokenizer`
// at the top of `llm/mod.rs`. Re-exported for the Part 2 marker bridge
// (`conversion/web_search.rs`) and these tests; unused until then.
#[allow(unused_imports)]
pub use agent_llm::web_search::{
	WebSearchTrigger, detect, is_web_search_server_tool, WEB_SEARCH_TYPES,
};

use crate::{apply, schema};
use crate::llm::RouteType;
use crate::types::agent::Target;

/// Per-request reroute signal inserted by `prepare_request` when web-search
/// triggers are detected *and* enabled on the target, consumed by
/// `make_backend_call` to swap the connection target to the sidecar. Mirrors
/// Kong's two-phase `ctx.web_search.{hijack, sidecar_url}` handoff:
/// `web-search-prepare.lua` sets it (body parsed → detect), `web-search-reroute.lua`
/// reads it (later stage → `kong.service.set_target`). Here `prepare_request`
/// sets it (via `parts.extensions`), and the httpproxy target-swap block reads it.
///
/// Carried on `http::request::Parts::extensions`; survives the
/// `Request::from_parts(parts, …)` rebuild in `process_chat_request` and the
/// LLM snapshot (which does NOT clear extensions), reaching the swap site.
///
/// Only the sidecar `target` is needed on the request side (the connection
/// swap). The response-side flags (`streaming`, `client_tools`) travel on
/// `LLMRequest.web_search` as a `WebSearchStreamContext` (set in
/// `prepare_request` from the same `WebSearchConfig`), since `LLMRequest`
/// lives in the `agent-llm` crate which cannot reference this `Target`-bearing
/// type.
#[derive(Clone, Debug)]
pub struct WebSearchReroute {
	/// Sidecar connection target (host:port), parsed from `sidecar_url`.
	pub target: Target,
}

impl WebSearchReroute {
	/// Parse a sidecar URL (`http(s)://host[:port][/path]`) into a reroute
	/// signal. Mirrors `web-search-prepare.lua parse_url`. The path is NOT
	/// carried here — the reroute always sends to `/v1/chat/completions`
	/// (the sidecar speaks OpenAI Chat), applied at the swap site. Returns
	/// `None` on an unparseable URL (treated as bypass — FR-7.10).
	pub fn from_sidecar_url(sidecar_url: &str) -> Option<Self> {
		let url = url::Url::parse(sidecar_url).ok()?;
		let host = url.host_str()?;
		// `url` accepts `http:///path` with an empty host — reject it (bypass).
		if host.is_empty() {
			return None;
		}
		let port = url.port_or_known_default().unwrap_or(80);
		Some(WebSearchReroute {
			target: Target::from((host, port)),
		})
	}
}

/// Map an agentgateway `RouteType` to the route-protocol string the web-search
/// sidecar expects in `x-ai-ws-config.route_type` (and echoes back in the
/// `x-ai-ws-provenance` marker). Mirrors Kong's `conf.route_type` values
/// (`"llm/v1/chat"`, `"llm/v1/messages"`, `"llm/v1/responses"`).
///
/// The sidecar speaks OpenAI Chat only; agentgateway reshapes the SSE on the
/// client side, so this string is load-bearing only for the provenance echo
/// (it rides through unchanged) — but we send the client-side route so the
/// echo round-trips to a value the shape layer can correlate.
pub(crate) fn route_type_string(route_type: RouteType) -> &'static str {
	match route_type {
		RouteType::Completions => "llm/v1/chat",
		RouteType::Messages | RouteType::AnthropicTokenCount => "llm/v1/messages",
		RouteType::Responses => "llm/v1/responses",
		// Non-chat routes never reach the reroute (detection is chat-only), but
		// map them to a sentinel rather than panic so the encoder is total.
		RouteType::Passthrough
		| RouteType::Detect
		| RouteType::Models
		| RouteType::Embeddings
		| RouteType::Realtime
		| RouteType::Rerank
		| RouteType::GenerateContent
		| RouteType::GeminiCountTokens => "llm/v1/chat",
	}
}

/// Configuration for the web-search sidecar. Set on `Policy.web_search` to
/// enable hijack+reroute on a model. Mirrors the effective fields of Kong's
/// `model.server_tools_config` / legacy `model.web_search` block.
#[apply(schema!)]
#[serde(default)]
pub struct WebSearchConfig {
	/// URL of the web-search sidecar. The reroute sends requests to
	/// `<sidecar_url>/v1/chat/completions`. Required to enable hijack.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub sidecar_url: Option<String>,

	/// Whether the sidecar streams (`data:` SSE) or buffers (one JSON body).
	/// Defaults to `true` (streaming). When `false`, a client `stream=true` is
	/// downgraded to `stream=false` and the buffered shaper runs. Mirrors
	/// `model.web_search.streaming` (default true).
	#[serde(default = "default_streaming", skip_serializing_if = "std::ops::Not::not")]
	pub streaming: bool,

	/// `lazy_defer` mode: defer same-batch server tool calls to the next turn
	/// (Anthropic parity). Defaults to `false`. Forwarded to the sidecar via
	/// `x-ai-ws-config.lazy_defer`.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub lazy_defer: bool,

	/// Forward the client's own function tools to the sidecar (default `true`).
	/// When `false`, only server-side web_search runs — a client tool call can
	/// never suspend the loop. Mirrors `ws_cfg.client_tools` (default true).
	#[serde(default = "default_client_tools", skip_serializing_if = "std::ops::Not::not")]
	pub client_tools: bool,

	/// Server tools enabled on this target. Mirrors `model.server_tools[]`
	/// entries (`{name, enabled}`). Currently only `web_search` is registered,
	/// but the list shape keeps the registry-driven extension path open
	/// (adding a tool = add a row to `WEB_SEARCH_TYPES` + a config entry).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub enabled_tools: Vec<EnabledServerTool>,
}

fn default_streaming() -> bool {
	true
}
fn default_client_tools() -> bool {
	true
}

impl Default for WebSearchConfig {
	fn default() -> Self {
		Self {
			sidecar_url: None,
			streaming: default_streaming(),
			lazy_defer: false,
			client_tools: default_client_tools(),
			enabled_tools: Vec::new(),
		}
	}
}

/// A server tool enabled on a target. Mirrors a `model.server_tools[]` entry.
#[apply(schema!)]
#[serde(default)]
pub struct EnabledServerTool {
	/// Registered tool name (e.g. `"web_search"`).
	pub name: String,
	/// Whether this tool is enabled on the target. Defaults to `true` when
	/// absent, mirroring `server_tools.enabled_on_target` (`entry.enabled ~= false`).
	#[serde(default = "default_enabled")]
	pub enabled: bool,
}

fn default_enabled() -> bool {
	true
}

impl Default for EnabledServerTool {
	fn default() -> Self {
		Self {
			name: String::new(),
			enabled: default_enabled(),
		}
	}
}

impl WebSearchConfig {
	/// Returns `true` if `web_search` is enabled on this target. Mirrors
	/// `server_tools.enabled_on_target`: a tool is enabled if (a) it appears in
	/// `enabled_tools` with `enabled != false`, or (b) `enabled_tools` is empty
	/// and `sidecar_url` is set (legacy `model.web_search.enabled` shape —
	/// enabling the config at all implies web_search).
	pub fn web_search_enabled(&self) -> bool {
		if self.sidecar_url.is_none() {
			return false;
		}
		if self.enabled_tools.is_empty() {
			// Legacy shape: configuring web_search at all implies web_search enabled.
			return true;
		}
		self.enabled_tools
			.iter()
			.any(|e| e.name == "web_search" && e.enabled)
	}

	/// Filter detected triggers to those enabled on this target. Mirrors
	/// `web-search-prepare.select_enabled(found, model_tbl)`. Returns the
	/// triggers the reroute should act on; if empty, the request stays on the
	/// normal upstream with no hijack (FR-7.10 bypass path).
	pub fn select_enabled<'a>(
		&self,
		found: &'a [WebSearchTrigger],
	) -> Vec<&'a WebSearchTrigger> {
		if !self.web_search_enabled() {
			return Vec::new();
		}
		// Only `web_search` is registered today. With explicit `enabled_tools`,
		// select triggers whose name is enabled; with the legacy shape (empty
		// `enabled_tools`), all `web_search` triggers are selected.
		if self.enabled_tools.is_empty() {
			return found.iter().filter(|t| t.name == "web_search").collect();
		}
		found
			.iter()
			.filter(|t| {
				self.enabled_tools
					.iter()
					.any(|e| e.name == t.name && e.enabled)
			})
			.collect()
	}
}

impl WebSearchConfig {
	/// Build the `x-ai-ws-config` header value (as a JSON value; the caller
	/// serializes and sets the header). Mirrors `web-search-prepare.lua:508-518`:
	/// `{tools:[{name, max_uses?, allowed_domains?, blocked_domains?,
	/// user_location?}], route_type, lazy_defer}`. Only ENABLED tools are
	/// forwarded. `route_type` is the client-side route-protocol string (see
	/// [`route_type_string`]); `lazy_defer` is the explicitly-on flag (default
	/// false). Returns `None` when no triggers are selected (the bypass path —
	/// FR-7.10 — sets no header and stays on the normal upstream).
	pub fn ws_config_value<'a, I>(
		&self,
		selected: I,
		route_type: RouteType,
	) -> Option<serde_json::Value>
	where
		I: IntoIterator<Item = &'a WebSearchTrigger>,
	{
		let mut tools: Vec<serde_json::Value> = Vec::new();
		for t in selected {
			let mut entry = serde_json::Map::new();
			entry.insert("name".into(), serde_json::json!(t.name));
			if let Some(max_uses) = t.max_uses {
				entry.insert("max_uses".into(), serde_json::json!(max_uses));
			}
			if let Some(allowed) = &t.allowed_domains {
				entry.insert("allowed_domains".into(), serde_json::json!(allowed));
			}
			if let Some(blocked) = &t.blocked_domains {
				entry.insert("blocked_domains".into(), serde_json::json!(blocked));
			}
			if let Some(loc) = &t.user_location {
				entry.insert("user_location".into(), loc.clone());
			}
			tools.push(serde_json::Value::Object(entry));
		}
		if tools.is_empty() {
			return None;
		}
		Some(serde_json::json!({
			"tools": tools,
			"route_type": route_type_string(route_type),
			"lazy_defer": self.lazy_defer,
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn select_enabled_legacy_shape_selects_when_sidecar_set() {
		let cfg = WebSearchConfig {
			sidecar_url: Some("http://sidecar".into()),
			..Default::default()
		};
		let found = detect(&[json!({"type": "web_search_20250305"})]);
		assert_eq!(cfg.select_enabled(&found).len(), 1);
	}

	#[test]
	fn select_enabled_no_sidecar_selects_nothing() {
		// No sidecar_url => no hijack, regardless of detection (bypass path).
		let cfg = WebSearchConfig::default();
		let found = detect(&[json!({"type": "web_search_20250305"})]);
		assert!(cfg.select_enabled(&found).is_empty());
	}

	#[test]
	fn select_enabled_respects_disabled_flag() {
		let cfg = WebSearchConfig {
			sidecar_url: Some("http://sidecar".into()),
			enabled_tools: vec![EnabledServerTool {
				name: "web_search".into(),
				enabled: false,
			}],
			..Default::default()
		};
		let found = detect(&[json!({"type": "web_search_20250305"})]);
		assert!(cfg.select_enabled(&found).is_empty());
	}

	#[test]
	fn select_enabled_explicit_tools_selects_matching() {
		let cfg = WebSearchConfig {
			sidecar_url: Some("http://sidecar".into()),
			enabled_tools: vec![EnabledServerTool {
				name: "web_search".into(),
				enabled: true,
			}],
			..Default::default()
		};
		let found = detect(&[json!({"type": "web_search_20260209"})]);
		assert_eq!(cfg.select_enabled(&found).len(), 1);
	}

	#[test]
	fn ws_config_value_encodes_selected_trigger_with_options() {
		let cfg = WebSearchConfig {
			sidecar_url: Some("http://sidecar".into()),
			lazy_defer: true,
			..Default::default()
		};
		let found = detect(&[json!({
			"type": "web_search_20250305",
			"max_uses": 5,
			"allowed_domains": ["example.com"],
			"blocked_domains": ["bad.example"],
			"user_location": {"city": "SF"}
		})]);
		let selected = cfg.select_enabled(&found);
		let v = cfg
			.ws_config_value(selected.iter().copied(), RouteType::Messages)
			.expect("non-empty selection encodes a config");
		assert_eq!(v["route_type"], "llm/v1/messages");
		assert_eq!(v["lazy_defer"], true);
		assert_eq!(v["tools"][0]["name"], "web_search");
		assert_eq!(v["tools"][0]["max_uses"], 5);
		assert_eq!(v["tools"][0]["allowed_domains"][0], "example.com");
		assert_eq!(v["tools"][0]["blocked_domains"][0], "bad.example");
		assert_eq!(v["tools"][0]["user_location"]["city"], "SF");
	}

	#[test]
	fn ws_config_value_omits_absent_optional_fields() {
		// A trigger with only `type` set must not emit null option fields.
		let cfg = WebSearchConfig {
			sidecar_url: Some("http://sidecar".into()),
			..Default::default()
		};
		let found = detect(&[json!({"type": "web_search"})]);
		let selected = cfg.select_enabled(&found);
		let v = cfg
			.ws_config_value(selected.iter().copied(), RouteType::Completions)
			.unwrap();
		let entry = &v["tools"][0];
		assert_eq!(entry["name"], "web_search");
		assert!(entry.get("max_uses").is_none());
		assert!(entry.get("allowed_domains").is_none());
		assert!(entry.get("blocked_domains").is_none());
		assert!(entry.get("user_location").is_none());
		assert_eq!(v["route_type"], "llm/v1/chat");
		assert_eq!(v["lazy_defer"], false);
	}

	#[test]
	fn ws_config_value_none_for_empty_selection() {
		// Bypass path (FR-7.10): no selected triggers => no header.
		let cfg = WebSearchConfig::default();
		let found = detect(&[json!({"type": "web_search_20250305"})]);
		let selected = cfg.select_enabled(&found);
		assert!(cfg.ws_config_value(selected.iter().copied(), RouteType::Messages).is_none());
	}

	#[test]
	fn route_type_string_maps_all_chat_routes() {
		assert_eq!(route_type_string(RouteType::Completions), "llm/v1/chat");
		assert_eq!(route_type_string(RouteType::Messages), "llm/v1/messages");
		assert_eq!(
			route_type_string(RouteType::AnthropicTokenCount),
			"llm/v1/messages"
		);
		assert_eq!(route_type_string(RouteType::Responses), "llm/v1/responses");
	}

	#[test]
	fn from_sidecar_url_parses_host_and_default_port() {
		let r = WebSearchReroute::from_sidecar_url("http://sidecar.local")
			.expect("valid url");
		assert!(matches!(r.target, crate::types::agent::Target::Hostname(h, 80) if h.as_str() == "sidecar.local"));
	}

	#[test]
	fn from_sidecar_url_parses_https_explicit_port() {
		let r = WebSearchReroute::from_sidecar_url("https://sidecar:8443/path")
			.expect("valid url");
		assert!(matches!(r.target, crate::types::agent::Target::Hostname(h, 8443) if h.as_str() == "sidecar"));
	}

	#[test]
	fn from_sidecar_url_rejects_invalid() {
		// Malformed URL → None (bypass path).
		assert!(WebSearchReroute::from_sidecar_url("not a url").is_none());
		// A URL with no host authority (mailto) → None.
		assert!(WebSearchReroute::from_sidecar_url("mailto:foo@bar").is_none());
		// A non-http(s) scheme that url::Url still parses but carries no
		// network host → None.
		assert!(WebSearchReroute::from_sidecar_url("file:///tmp/x").is_none());
	}
}
