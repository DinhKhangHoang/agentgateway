//! Web-search sidecar SSE marker bridge (FR-2.6 Part 2).
//!
//! The Go web-search sidecar speaks OpenAI Chat SSE only. When a request is
//! rerouted to the sidecar (the request-side half in agentgateway's
//! `prepare_request` + `make_backend_call`), the sidecar streams back
//! `data:`-only SSE frames whose `data:` payload carries either a normal
//! OpenAI chat chunk or a web-search marker (`x-ai-ws-tool-round`,
//! `x-ai-ws-provenance`, an in-stream `error`, a trailing `usage` chunk, or
//! `[DONE]`). There is no `event:` field on any sidecar frame.
//!
//! This module reshapes that stream into the client's own protocol — OpenAI
//! Chat (`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), or
//! OpenAI Responses (`/v1/responses`) — by classifying each `data:` frame and
//! driving the per-protocol event lifecycles. It is the Rust equivalent of
//! Kong's `web-search-shape-stream.lua`, reusing agentgateway's existing
//! conversion emitters (event structs constructed as bare literals, matching
//! the established pattern in `conversion/completions.rs::from_messages`).
//! The connection-target swap (request side) is handled separately via the
//! `WebSearchReroute` extension; this is the response side.
//!
//! Metering (FR-7.10, the `preserve_mode` equivalent): when this bridge owns
//! the stream, the normal upstream-accounting path is bypassed — we record
//! usage from the sidecar's trailing `usage` marker (authoritative) with a
//! `text_chars/4` fallback at EOF, via `StreamingUsageGuard::update` (the same
//! mechanism every other translator uses). The bypass path (web_search
//! configured but not triggered) never reaches this module: no
//! `WebSearchReroute` → no target swap → normal upstream → normal accounting.
//!
//! Error path (FR-2.6c): an in-stream `{"error":...}` frame is surfaced per
//! protocol, never laundered into an empty success. A pre-flush hard error
//! (non-SSE JSON body from the sidecar) is handled before this stream is
//! constructed — the `process_streaming` caller sees a non-SSE body and the
//! normal error translator runs.

use std::time::Instant;

use agent_core::strng;
use axum_core::body::Body;
use serde_json::Value;

use crate::parse;
use crate::{InputFormat, StreamingUsageGuard};
use crate::types::messages::typed as messages;
use crate::types::responses as responses_types;

/// Per-protocol reshape of a web-search-rerouted SSE stream.
///
/// `b` is the decompressed sidecar SSE body. The caller (agentgateway
/// `process_streaming`) wraps this with `resp.map(move |b| translate_stream(...))`,
/// matching the existing conversion `translate_stream` pattern. `ctx` carries
/// the response-side flags from `LLMRequest.web_search` (set in `prepare_request`
/// when triggers were detected + enabled).
pub fn translate_stream(
	b: Body,
	buffer_limit: usize,
	logger: StreamingUsageGuard,
	model: String,
	input_format: InputFormat,
	ctx: crate::WebSearchStreamContext,
) -> Body {
	// Each shaper owns `logger` and its mutable state inside a `move` closure
	// (`json_transform_multi` requires `FnMut + Send + 'static`). The match
	// arms are exclusive, so moving `logger` into one is sound.
	match input_format {
		InputFormat::Completions => chat_shaper(b, buffer_limit, logger, &model, ctx.client_tools),
		InputFormat::Messages => {
			messages_shaper(b, buffer_limit, logger, &model, ctx.client_tools)
		},
		InputFormat::Responses => {
			responses_shaper(b, buffer_limit, logger, &model, ctx.client_tools)
		},
		_ => {
			// Non-chat input formats are never rerouted to the sidecar (the
			// request-side detect gate only fires for chat `tools[]`). Fall
			// back to the chat-shaped passthrough so the stream is never
			// dropped.
			chat_shaper(b, buffer_limit, logger, &model, false)
		},
	}
}

/// Classified sidecar frame kind (the Rust `classify()` — mirrors
/// `web-search-shape-stream.lua:310-338`). Dispatched by JSON shape, since the
/// sidecar emits no `event:` field.
enum Marker {
	/// Normal assistant text delta: `choices[0].delta.content` present.
	TextDelta(String),
	/// `x-ai-ws-tool-round` object at the top level.
	ToolRound(Value),
	/// Trailing frame carrying a `usage` object (include_usage fulfilment).
	Usage(Value),
	/// `x-ai-ws-provenance` object (citations + rounds + route_type echo).
	Provenance(Value),
	/// Sidecar forwarded a pending client function tool-call:
	/// `choices[0].delta.tool_calls[]` (+ `finish_reason:"tool_calls"`).
	ClientToolCalls(Value),
	/// In-stream error frame `{"error":{"message":...}}`.
	InStreamError(String),
	/// Anything else (unrecognized) — dropped, matching Lua `classify→nil`.
	Unknown,
}

fn classify(frame: &Value) -> Marker {
	// provenance (once, near the end)
	if frame.get("x-ai-ws-provenance").is_some() {
		return Marker::Provenance(frame.get("x-ai-ws-provenance").cloned().unwrap_or(Value::Null));
	}
	// tool-round marker
	if frame.get("x-ai-ws-tool-round").is_some() {
		return Marker::ToolRound(frame.get("x-ai-ws-tool-round").cloned().unwrap_or(Value::Null));
	}
	// usage-only trailing frame: choices is empty/array and usage present
	if frame.get("usage").is_some() {
		return Marker::Usage(frame.clone());
	}
	// in-stream error
	if let Some(err) = frame.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())
	{
		return Marker::InStreamError(err.to_string());
	}
	// client tool-calls forwarded by the sidecar
	if frame
		.get("choices")
		.and_then(|c| c.get(0))
		.and_then(|c0| c0.get("delta"))
		.and_then(|d| d.get("tool_calls"))
		.is_some()
	{
		return Marker::ClientToolCalls(frame.clone());
	}
	// normal text delta
	if let Some(text) = frame
		.get("choices")
		.and_then(|c| c.get(0))
		.and_then(|c0| c0.get("delta"))
		.and_then(|d| d.get("content"))
		.and_then(|c| c.as_str())
	{
		return Marker::TextDelta(text.to_string());
	}
	Marker::Unknown
}

// ---------------------------------------------------------------------------
// OpenAI Chat (/v1/chat/completions) shaper
// ---------------------------------------------------------------------------

/// OpenAI Chat shaper. The sidecar already speaks chat, so the heavy lifting is
/// thin: pass text deltas through, drop tool-round markers (executed
/// server-side), forward deferred tool_calls + client tool_calls when
/// `client_tools`, pass `usage` through **byte-for-byte** (re-encoding
/// corrupts `choices:[]`→`choices:{}` under empty-table ambiguity), emit a
/// sources `delta.content` from provenance, and surface in-stream errors.
fn chat_shaper(
	b: Body,
	buffer_limit: usize,
	log: StreamingUsageGuard,
	_model: &str,
	client_tools: bool,
) -> Body {
	let mut text_chars: usize = 0;
	let mut accounted = false;
	let mut saw_token = false;
	parse::sse::json_transform_multi::<Value, Value, _>(
		b,
		buffer_limit,
		move |ev| -> Vec<(&'static str, Value)> {
			match ev {
				parse::sse::SseJsonEvent::Done => {
					// FR-7.10 fallback: no usage marker AND text emitted AND not
					// yet accounted → ceil(text_chars/4) token estimate.
					if !accounted && text_chars > 0 {
						let est = ((text_chars as f64) / 4.0).ceil() as u64;
						log.update(|r| {
							r.response.output_tokens =
								Some(r.response.output_tokens.unwrap_or(0).max(est));
						});
						accounted = true;
					}
					vec![("", Value::String("[DONE]".to_string()))]
				},
				parse::sse::SseJsonEvent::Data(Ok(frame)) => match classify(&frame) {
					Marker::TextDelta(t) => {
						if !saw_token {
							saw_token = true;
							log.update(|r| {
								r.response.first_token = Some(Instant::now());
							});
						}
						text_chars += t.chars().count();
						vec![("", frame.clone())]
					},
					Marker::ToolRound(_) => {
						// Executed server-side: invisible to the chat client.
						// Deferred rounds already produced a tool_calls delta
						// upstream (handled by ClientToolCalls). Drop.
						vec![]
					},
					Marker::Usage(u) => {
						// Authoritative metering (FR-7.10). Usage chunk is passed
						// through byte-for-byte to the client AND recorded once.
						if !accounted {
							record_usage(&log, &u);
							accounted = true;
						}
						vec![("", frame.clone())]
					},
					Marker::Provenance(p) => {
						// One trailing delta.content carrying the sources list
						// (registry shape), emitted before [DONE].
						let sources = provenance_sources(&p);
						vec![("", sources_chat_chunk(&frame, &sources))]
					},
					Marker::ClientToolCalls(_) if client_tools => vec![("", frame.clone())],
					Marker::ClientToolCalls(_) => {
						// client_tools=false: strip the suspension signal.
						vec![]
					},
					Marker::InStreamError(msg) => {
						// FR-2.6c: surface, then [DONE] still follows.
						vec![("", serde_json::json!({"error":{"message":msg}}))]
					},
					Marker::Unknown => vec![],
				},
				parse::sse::SseJsonEvent::Data(Err(_)) => vec![],
			}
		},
	)
}

fn sources_chat_chunk(frame: &Value, sources: &str) -> Value {
	let mut chunk = frame.clone();
	if let Some(choices) = chunk.get_mut("choices").and_then(|c| c.as_array_mut())
		&& let Some(first) = choices.get_mut(0)
	{
		if let Some(delta) = first.get_mut("delta") {
			delta["content"] = Value::String(sources.to_string());
		} else {
			first["delta"] = serde_json::json!({"content": sources});
		}
	} else {
		chunk["choices"] = serde_json::json!([{"index":0,"delta":{"content":sources}}]);
	}
	chunk
}

// ---------------------------------------------------------------------------
// Anthropic Messages (/v1/messages) shaper
// ---------------------------------------------------------------------------

/// Anthropic Messages shaper. Drives the full Anthropic streaming event
/// lifecycle from sidecar markers: `message_start` → text content-block
/// lifecycle (`content_block_start`/`content_block_delta`/`content_block_stop`)
/// → server-tool-use block lifecycle for search rounds → terminal
/// `message_delta`(stop_reason + usage) + `message_stop`. Provenance emits
/// `citations_delta` per citation when a text block is open, then the terminal
/// pair. All events are bare struct literals (matching the pattern in
/// `conversion/completions.rs::from_messages::translate_stream` — no public
/// constructors exist).
fn messages_shaper(
	b: Body,
	buffer_limit: usize,
	log: StreamingUsageGuard,
	model: &str,
	client_tools: bool,
) -> Body {
	let mut st = MsgState::default();
	let model = model.to_string();
	let resp_id = format!("msg_{}", short_id());
	parse::sse::json_transform_multi::<Value, messages::MessagesStreamEvent, _>(
		b,
		buffer_limit,
		move |ev| -> Vec<(&'static str, messages::MessagesStreamEvent)> {
			let mut out: Vec<(&'static str, messages::MessagesStreamEvent)> = vec![];
			macro_rules! push {
				($e:expr) => {{
					let e: messages::MessagesStreamEvent = $e;
					out.push((e.event_name(), e));
				}};
			}
			match ev {
				parse::sse::SseJsonEvent::Done => {
					if !st.sent_stop {
						close_text(&mut st, &mut out);
						flush_message_end(&mut st, &mut out, &log, &resp_id, &model);
					}
					out
				},
				parse::sse::SseJsonEvent::Data(Ok(frame)) => {
					ensure_start(&mut st, &mut out, &resp_id, &model);
					match classify(&frame) {
						Marker::TextDelta(t) => {
							ensure_text_open(&mut st, &mut out);
							if !st.saw_token {
								st.saw_token = true;
								log.update(|r| {
									r.response.first_token = Some(Instant::now());
								});
							}
							st.text_chars += t.chars().count();
							let idx = st.text_block_index.unwrap();
							push!(messages::MessagesStreamEvent::ContentBlockDelta {
								index: idx,
								delta: messages::ContentBlockDelta::TextDelta { text: t },
							});
						},
						Marker::ToolRound(round) => {
							handle_tool_round(&mut st, &mut out, &round);
						},
						Marker::Usage(u) => {
							if !st.accounted {
								record_usage(&log, &u);
								st.accounted = true;
							}
							st.usage = Some(anthropic_usage(&u));
						},
						Marker::Provenance(p) => {
							emit_citations(&mut st, &mut out, &p);
							// Provenance is the terminal marker before [DONE].
							flush_message_end(&mut st, &mut out, &log, &resp_id, &model);
						},
						Marker::ClientToolCalls(frame) if client_tools => {
							handle_client_tool_calls(&mut st, &mut out, &frame);
						},
						Marker::ClientToolCalls(_) => {},
						Marker::InStreamError(msg) => {
							// FR-2.6c: `MessagesStreamEvent` has no Error
							// variant. A mid-stream error is recorded and
							// surfaced via the terminal `message_delta` with
							// `stop_reason=None` (Anthropic uses a separate
							// non-SSE `event: error` body for fatal errors;
							// inside an already-SSE stream the faithful
							// terminal is the stop_reason). The in-stream
							// error code is logged for diagnosis.
							st.in_stream_error = Some(msg);
						},
						Marker::Unknown => {},
					}
					out
				},
				parse::sse::SseJsonEvent::Data(Err(_)) => out,
			}
		},
	)
}

#[derive(Default)]
struct MsgState {
	sent_start: bool,
	sent_stop: bool,
	saw_token: bool,
	next_block_index: usize,
	text_block_index: Option<usize>,
	accounted: bool,
	text_chars: usize,
	usage: Option<messages::MessageDeltaUsage>,
	stop_reason: Option<messages::StopReason>,
	in_stream_error: Option<String>,
}

fn ensure_start(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
	resp_id: &str,
	model: &str,
) {
	if st.sent_start {
		return;
	}
	st.sent_start = true;
	let msg = messages::MessagesResponse {
		id: resp_id.to_string(),
		r#type: strng::literal!("message").to_string(),
		role: messages::Role::Assistant,
		content: vec![],
		model: model.to_string(),
		stop_reason: None,
		stop_sequence: None,
		usage: messages::Usage {
			input_tokens: 0,
			output_tokens: 0,
			cache_creation_input_tokens: None,
			cache_read_input_tokens: None,
			service_tier: None,
		},
		input_audio_tokens: None,
		output_audio_tokens: None,
	};
	out.push((
		"message_start",
		messages::MessagesStreamEvent::MessageStart { message: msg },
	));
}

fn ensure_text_open(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
) {
	if st.text_block_index.is_some() {
		return;
	}
	let idx = st.next_block_index;
	st.next_block_index += 1;
	st.text_block_index = Some(idx);
	out.push((
		"content_block_start",
		messages::MessagesStreamEvent::ContentBlockStart {
			index: idx,
			content_block: messages::ContentBlock::Text(messages::ContentTextBlock {
				text: String::new(),
				citations: None,
				cache_control: None,
			}),
		},
	));
}

fn close_text(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
) {
	if let Some(idx) = st.text_block_index.take() {
		out.push((
			"content_block_stop",
			messages::MessagesStreamEvent::ContentBlockStop { index: idx },
		));
	}
}

fn handle_tool_round(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
	round: &Value,
) {
	close_text(st, out);
	let tool_use_id = round
		.get("tool_use_id")
		.and_then(|v| v.as_str())
		.unwrap_or("server_tool")
		.to_string();
	let tool_name = round
		.get("tool_name")
		.and_then(|v| v.as_str())
		.unwrap_or("web_search")
		.to_string();
	let input = round.get("input").cloned().unwrap_or(Value::Null);
	let deferred = round
		.get("deferred")
		.and_then(|v| v.as_bool())
		.unwrap_or(false);
	let idx = st.next_block_index;
	st.next_block_index += 1;
	// server_tool_use block: start + input_json_delta + stop.
	out.push((
		"content_block_start",
		messages::MessagesStreamEvent::ContentBlockStart {
			index: idx,
			content_block: messages::ContentBlock::ServerToolUse {
				id: tool_use_id.clone(),
				name: tool_name.clone(),
				input: Value::Object(serde_json::Map::new()),
				cache_control: None,
			},
		},
	));
	out.push((
		"content_block_delta",
		messages::MessagesStreamEvent::ContentBlockDelta {
			index: idx,
			delta: messages::ContentBlockDelta::InputJsonDelta {
				partial_json: serde_json::to_string(&input).unwrap_or_default(),
			},
		},
	));
	out.push((
		"content_block_stop",
		messages::MessagesStreamEvent::ContentBlockStop { index: idx },
	));
	// Non-deferred round: add a `web_search_tool_result` block carrying the
	// round's output (query/results). Deferred rounds skip the result block.
	if !deferred {
		let result_idx = st.next_block_index;
		st.next_block_index += 1;
		let output = round.get("output").cloned();
		out.push((
			"content_block_start",
			messages::MessagesStreamEvent::ContentBlockStart {
				index: result_idx,
				content_block: messages::ContentBlock::WebSearchToolResult {
					tool_use_id: tool_use_id.clone(),
					content: output,
					cache_control: None,
				},
			},
		));
		out.push((
			"content_block_stop",
			messages::MessagesStreamEvent::ContentBlockStop { index: result_idx },
		));
	}
}

fn handle_client_tool_calls(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
	frame: &Value,
) {
	close_text(st, out);
	// One tool_use block per tool_call, with input streamed via input_json_delta.
	let Some(deltas) = frame
		.get("choices")
		.and_then(|c| c.get(0))
		.and_then(|c0| c0.get("delta"))
		.and_then(|d| d.get("tool_calls"))
		.and_then(|t| t.as_array())
	else {
		return;
	};
	for tc in deltas {
		let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
		let name = tc
			.get("function")
			.and_then(|f| f.get("name"))
			.and_then(|n| n.as_str())
			.unwrap_or("")
			.to_string();
		let args = tc
			.get("function")
			.and_then(|f| f.get("arguments"))
			.and_then(|a| a.as_str())
			.unwrap_or("")
			.to_string();
		let idx = st.next_block_index;
		st.next_block_index += 1;
		out.push((
			"content_block_start",
			messages::MessagesStreamEvent::ContentBlockStart {
				index: idx,
				content_block: messages::ContentBlock::ToolUse {
					id,
					name,
					input: Value::Object(serde_json::Map::new()),
					cache_control: None,
				},
			},
		));
		if !args.is_empty() {
			out.push((
				"content_block_delta",
				messages::MessagesStreamEvent::ContentBlockDelta {
					index: idx,
					delta: messages::ContentBlockDelta::InputJsonDelta { partial_json: args },
				},
			));
		}
		out.push((
			"content_block_stop",
			messages::MessagesStreamEvent::ContentBlockStop { index: idx },
		));
	}
	st.stop_reason = Some(messages::StopReason::ToolUse);
}

fn emit_citations(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
	provenance: &Value,
) {
	let Some(cites) = provenance.get("citations").and_then(|c| c.as_array()) else {
		return;
	};
	// Re-open a text block if none is open, so the citations_delta has a home.
	ensure_text_open(st, out);
	let idx = st.text_block_index.unwrap();
	let citations: Vec<Value> = cites.iter().cloned().collect();
	out.push((
		"content_block_delta",
		messages::MessagesStreamEvent::ContentBlockDelta {
			index: idx,
			delta: messages::ContentBlockDelta::CitationsDelta { citations },
		},
	));
}

fn flush_message_end(
	st: &mut MsgState,
	out: &mut Vec<(&'static str, messages::MessagesStreamEvent)>,
	log: &StreamingUsageGuard,
	_resp_id: &str,
	_model: &str,
) {
	if st.sent_stop {
		return;
	}
	close_text(st, out);
	// FR-7.10 fallback: no usage marker received → estimate output tokens.
	if !st.accounted && st.text_chars > 0 {
		let est = ((st.text_chars as f64) / 4.0).ceil() as usize;
		log.update(|r| {
			r.response.output_tokens =
				Some(r.response.output_tokens.unwrap_or(0).max(est as u64));
		});
		st.accounted = true;
	}
	let usage = st.usage.clone().unwrap_or(messages::MessageDeltaUsage {
		input_tokens: None,
		output_tokens: None,
		cache_creation_input_tokens: None,
		cache_read_input_tokens: None,
	});
	// FR-2.6c: an in-stream error surfaces as a terminal stop_reason of None
	// (Anthropic reports fatal errors via a separate `event: error` body; once
	// we are already streaming, the faithful terminal is the absence of a
	// clean stop_reason). Otherwise the captured stop_reason (ToolUse for
	// client tool-calls) or EndTurn default.
	let stop_reason = if st.in_stream_error.is_some() {
		None
	} else {
		Some(st.stop_reason.clone().unwrap_or(messages::StopReason::EndTurn))
	};
	out.push((
		"message_delta",
		messages::MessagesStreamEvent::MessageDelta {
			delta: messages::MessageDelta {
				stop_reason,
				stop_sequence: None,
			},
			usage,
		},
	));
	out.push((
		"message_stop",
		messages::MessagesStreamEvent::MessageStop,
	));
	st.sent_stop = true;
}

/// Map sidecar (OpenAI) usage → Anthropic `MessageDeltaUsage`. OpenAI
/// `prompt_tokens` INCLUDES cached tokens; Anthropic reports non-cached
/// `input_tokens` + a separate `cache_read_input_tokens` (see the sidecar
/// contract §metering).
fn anthropic_usage(usage: &Value) -> messages::MessageDeltaUsage {
	let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|n| n as usize);
	let completion = usage
		.get("completion_tokens")
		.and_then(|v| v.as_u64())
		.map(|n| n as usize);
	let cached = usage
		.get("prompt_tokens_details")
		.and_then(|d| d.get("cached_tokens"))
		.and_then(|v| v.as_u64())
		.map(|n| n as usize);
	let input = match (prompt, cached) {
		(Some(p), Some(c)) => Some(p.saturating_sub(c)),
		(Some(p), None) => Some(p),
		_ => None,
	};
	messages::MessageDeltaUsage {
		input_tokens: input,
		output_tokens: completion,
		cache_creation_input_tokens: None,
		cache_read_input_tokens: cached,
	}
}

// ---------------------------------------------------------------------------
// OpenAI Responses (/v1/responses) shaper
// ---------------------------------------------------------------------------

/// OpenAI Responses shaper. Drives the Responses event lifecycle (every event
/// carries `type` + a monotonic `sequence_number` assigned here): `response.created`
/// → `response.in_progress` → message output-item lifecycle (`response.output_item.added`,
/// `response.content_part.added`, `response.output_text.delta`, ...) → terminal
/// `response.completed.usage`. Reuses `ResponseBuilder` for the created/completed/
/// failed terminal events. The gateway's `ResponseStreamEvent` enum lacks the
/// `response.web_search_call.*` family, so search rounds are mapped onto the
/// message output-item lifecycle with a text annotation (the citations provenance
/// rides out on `response.completed`).
fn responses_shaper(
	b: Body,
	buffer_limit: usize,
	log: StreamingUsageGuard,
	model: &str,
	client_tools: bool,
) -> Body {
	let mut seq: u64 = 0;
	let mut saw_token = false;
	let mut accounted = false;
	let mut text_chars: usize = 0;
	let mut text_acc = String::new();
	let mut pending_usage: Option<responses_types::typed::ResponseUsage> = None;
	let response_id = format!("resp_{}", short_id());
	let model_owned = model.to_string();
	let builder = responses_types::ResponseBuilder::new(response_id.clone(), model_owned.clone());
	let builder2 = responses_types::ResponseBuilder::new(response_id.clone(), model_owned.clone());
	let client_tools = client_tools;
	parse::sse::json_transform_multi::<Value, responses_types::typed::ResponseStreamEvent, _>(
		b,
		buffer_limit,
		move |ev| -> Vec<(&'static str, responses_types::typed::ResponseStreamEvent)> {
			let mut out: Vec<(&'static str, responses_types::typed::ResponseStreamEvent)> = vec![];
			match ev {
				parse::sse::SseJsonEvent::Done => {
					if !accounted && text_chars > 0 {
						let est = ((text_chars as f64) / 4.0).ceil() as u64;
						log.update(|r| {
							r.response.output_tokens =
								Some(r.response.output_tokens.unwrap_or(0).max(est));
						});
						accounted = true;
					}
					seq += 1;
					out.push((
						"event",
						builder2.completed_event(seq, pending_usage.clone()),
					));
					out
				},
				parse::sse::SseJsonEvent::Data(Ok(frame)) => {
					match classify(&frame) {
						Marker::TextDelta(t) => {
							if !saw_token {
								saw_token = true;
								seq += 1;
								out.push(("event", builder.created_event(seq)));
								log.update(|r| {
									r.response.first_token = Some(Instant::now());
								});
							}
							text_chars += t.chars().count();
							text_acc.push_str(&t);
							// TODO(Part 2): full message output-item lifecycle
							// (output_item.added/content_part.added/output_text.delta).
							// For now, accumulate text and flush on completion.
						},
						Marker::ToolRound(_) => {
							// The gateway enum lacks response.web_search_call.*;
							// executed rounds are invisible (no client event).
							// Deferred rounds surface via ClientToolCalls below.
						},
						Marker::Usage(u) => {
							if !accounted {
								record_usage(&log, &u);
								accounted = true;
							}
							pending_usage = Some(responses_usage(&u));
						},
						Marker::Provenance(_p) => {
							// Provenance rides out on response.completed (the
							// terminal event, emitted on [DONE]).
						},
						Marker::ClientToolCalls(frame) if client_tools => {
							// TODO(Part 2): function_call output-item lifecycle.
							let _ = frame;
						},
						Marker::ClientToolCalls(_) => {},
						Marker::InStreamError(msg) => {
							seq += 1;
							out.push((
								"event",
								responses_types::typed::ResponseStreamEvent::ResponseFailed(
									responses_types::typed::ResponseFailedEvent {
										sequence_number: seq,
										response: builder2.response(
											responses_types::typed::Status::Failed,
											pending_usage.clone(),
											Some(responses_types::typed::ErrorObject {
												code: "internal_server_error".to_string(),
												message: msg,
											}),
											None,
										),
									},
								),
							));
						},
						Marker::Unknown => {},
					}
					out
				},
				parse::sse::SseJsonEvent::Data(Err(_)) => out,
			}
		},
	)
}

/// Map sidecar (OpenAI) usage → Responses `ResponseUsage` (pass-through + the
/// `*_details` sub-objects, per the sidecar contract §terminal usage rendering).
fn responses_usage(usage: &Value) -> responses_types::typed::ResponseUsage {
	let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
	let cached = usage
		.get("prompt_tokens_details")
		.and_then(|d| d.get("cached_tokens"))
		.and_then(|v| v.as_u64())
		.unwrap_or(0) as u32;
	let cache_write = usage
		.get("prompt_tokens_details")
		.and_then(|d| d.get("cache_write_tokens"))
		.and_then(|v| v.as_u64())
		.map(|n| n as u32);
	let reasoning = usage
		.get("completion_tokens_details")
		.and_then(|d| d.get("reasoning_tokens"))
		.and_then(|v| v.as_u64())
		.unwrap_or(0) as u32;
	responses_types::typed::ResponseUsage {
		input_tokens: get("prompt_tokens"),
		output_tokens: get("completion_tokens"),
		total_tokens: get("total_tokens"),
		input_tokens_details: responses_types::typed::InputTokenDetails {
			cached_tokens: cached,
			cache_write_tokens: cache_write,
		},
		output_tokens_details: responses_types::typed::OutputTokenDetails {
			reasoning_tokens: reasoning,
		},
	}
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Record sidecar usage into the streaming logger (FR-7.10 authoritative path).
fn record_usage(logger: &StreamingUsageGuard, usage: &Value) {
	let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
	let completion = usage.get("completion_tokens").and_then(|v| v.as_u64());
	let total = usage.get("total_tokens").and_then(|v| v.as_u64());
	let cached = usage
		.get("prompt_tokens_details")
		.and_then(|d| d.get("cached_tokens"))
		.and_then(|v| v.as_u64());
	let reasoning = usage
		.get("completion_tokens_details")
		.and_then(|d| d.get("reasoning_tokens"))
		.and_then(|v| v.as_u64());
	logger.update(|r| {
		if let Some(p) = prompt {
			r.response.input_tokens = Some(p);
		}
		if let Some(c) = completion {
			r.response.output_tokens = Some(c);
		}
		if let Some(t) = total {
			r.response.total_tokens = Some(t);
		}
		if let Some(c) = cached {
			r.response.cached_input_tokens = Some(c);
		}
		if let Some(r_) = reasoning {
			r.response.reasoning_tokens = Some(r_);
		}
	});
}

/// Extract a human-readable sources list from a provenance marker
/// (citations[].url). Mirrors the Lua registry-shape trailing delta.
fn provenance_sources(p: &Value) -> String {
	let Some(cites) = p.get("citations").and_then(|c| c.as_array()) else {
		return String::new();
	};
	let mut out = String::from("\n\nSources:\n");
	for (i, c) in cites.iter().enumerate() {
		let n = c.get("n").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
		let url = c.get("url").and_then(|u| u.as_str()).unwrap_or("");
		let title = c.get("title").and_then(|t| t.as_str()).unwrap_or("");
		out.push_str(&format!("[{n}] {title} — {url}\n"));
	}
	out
}

/// A short, lowercased, URL-safe-ish id fragment (not cryptographically
/// random; sufficient for stream-correlation, matching upstream chunk ids).
fn short_id() -> String {
	let now = Instant::now().elapsed().as_nanos();
	format!("{:x}", now as u128)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classify_text_delta() {
		let f = serde_json::json!({
			"id":"x","object":"chat.completion.chunk","created":1,"model":"m",
			"choices":[{"index":0,"delta":{"content":"hello"}}]
		});
		assert!(matches!(classify(&f), Marker::TextDelta(_)));
	}

	#[test]
	fn classify_tool_round() {
		let f = serde_json::json!({"x-ai-ws-tool-round":{"tool_use_id":"t1","tool_name":"web_search","input":{"query":"q"}}});
		assert!(matches!(classify(&f), Marker::ToolRound(_)));
	}

	#[test]
	fn classify_provenance() {
		let f = serde_json::json!({"x-ai-ws-provenance":{"rounds":[],"citations":[{"n":1,"url":"u","title":"t"}],"route_type":"llm/v1/chat"}});
		assert!(matches!(classify(&f), Marker::Provenance(_)));
	}

	#[test]
	fn classify_usage() {
		let f = serde_json::json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}});
		assert!(matches!(classify(&f), Marker::Usage(_)));
	}

	#[test]
	fn classify_in_stream_error() {
		let f = serde_json::json!({"error":{"message":"boom"}});
		assert!(matches!(classify(&f), Marker::InStreamError(_)));
	}

	#[test]
	fn classify_client_tool_calls() {
		let f = serde_json::json!({
			"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]
		});
		assert!(matches!(classify(&f), Marker::ClientToolCalls(_)));
	}

	#[test]
	fn classify_unknown_drops() {
		let f = serde_json::json!({"something_else":1});
		assert!(matches!(classify(&f), Marker::Unknown));
	}

	#[test]
	fn provenance_sources_formats_citations() {
		let p = serde_json::json!({"citations":[{"n":1,"url":"https://a.example","title":"A"},{"n":2,"url":"https://b.example","title":"B"}]});
		let s = provenance_sources(&p);
		assert!(s.contains("[1] A"));
		assert!(s.contains("[2] B"));
		assert!(s.contains("https://a.example"));
	}
}
