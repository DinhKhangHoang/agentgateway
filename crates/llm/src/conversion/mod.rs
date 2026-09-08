use bytes::Bytes;

/// Message text for a client-facing error synthesized from an upstream body no
/// translator could parse.
///
/// Not every error body on the wire comes from the model server: a reverse
/// proxy, load balancer or WAF in front of it answers with its own HTML or
/// plain-text page. A strict parse of that used to fail the whole exchange with
/// `AIError::ResponseParsing`, which surfaces as a gateway 503 -- destroying the
/// upstream status that `traffic.retry` and a health policy's
/// `unhealthyCondition` are both evaluated against, and handing the client an
/// error body in no API's shape. Carrying the body through as text keeps the
/// failure diagnosable; the cap keeps a large error page out of the client's
/// error object, and collapsing whitespace keeps a multi-line HTML page
/// readable in a log line.
pub fn unparseable_upstream_body(bytes: &Bytes) -> String {
	const LIMIT: usize = 512;
	let text = String::from_utf8_lossy(bytes);
	let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
	if collapsed.is_empty() {
		return "upstream returned an empty error body".to_string();
	}
	if collapsed.chars().count() <= LIMIT {
		return collapsed;
	}
	let truncated: String = collapsed.chars().take(LIMIT).collect();
	format!("{truncated}... (truncated)")
}

pub mod bedrock;
pub mod completions;
pub mod gemini;
pub mod messages;
pub mod openai_compat;
pub mod responses;
pub mod vertex;
pub mod vertex_gemini;
pub mod web_search;

#[cfg(test)]
mod rerank_tests;
