package main

import (
	"encoding/json"
	"strings"
)

// maxPromptTextBytes bounds the text handed to the guardrail matcher. A prompt
// larger than this is truncated for MATCHING purposes only -- the body itself is
// forwarded verbatim. Bounded so a pathological request cannot turn keyword
// matching into a CPU denial of service.
const maxPromptTextBytes = 1 << 20 // 1 MiB

// maxWalkDepth stops a maliciously nested JSON document from blowing the stack
// during text collection.
const maxWalkDepth = 16

// RequestFacts is everything the processor needs from the client's JSON body.
type RequestFacts struct {
	// Model is the exact `model` string. It MUST be forwarded byte-identical to
	// /v1/check and /v1/usage: the plugin server keys both the guardrail map and
	// the per-model TPM counters on this exact string, so any normalisation here
	// would silently split a tenant's counters in two.
	Model string
	// Prompt is the concatenated user-supplied text, for guardrail matching only.
	// It is NEVER logged.
	Prompt string
	// Stream reports whether the client asked for `"stream": true`. Advisory
	// only -- the authoritative signal is the response content-type.
	Stream bool
}

// ExtractRequestFacts parses an OpenAI-compatible request body. It handles the
// chat shape (`messages[]`, content as a string or as an array of parts) and the
// completion/responses shapes (`prompt`, `input`), because the pilot's models
// are addressed through all of them.
//
// A body that is not JSON at all yields a zero RequestFacts and no error: it is
// not this service's job to validate the provider's schema, and rejecting an
// unparseable body here would break any provider-specific payload the gateway
// happily proxies today.
func ExtractRequestFacts(body []byte) RequestFacts {
	var doc map[string]json.RawMessage
	if err := json.Unmarshal(body, &doc); err != nil {
		return RequestFacts{}
	}

	facts := RequestFacts{}
	if raw, ok := doc["model"]; ok {
		var s string
		if json.Unmarshal(raw, &s) == nil {
			facts.Model = s
		}
	}
	if raw, ok := doc["stream"]; ok {
		var b bool
		if json.Unmarshal(raw, &b) == nil {
			facts.Stream = b
		}
	}

	var sb strings.Builder
	for _, field := range []string{"messages", "input", "prompt"} {
		raw, ok := doc[field]
		if !ok {
			continue
		}
		var v any
		if err := json.Unmarshal(raw, &v); err != nil {
			continue
		}
		collectText(v, &sb, 0)
	}
	facts.Prompt = sb.String()
	return facts
}

// collectText walks a decoded JSON value appending every human-readable string
// it can find to sb.
//
// It is deliberately BROAD rather than precise: for guardrail matching a false
// positive (scanning a string that was not really prompt text) is harmless,
// while a false negative (missing text because the provider used a shape we did
// not anticipate) is a bypass. It skips only the keys that are structural
// metadata and never carry user text -- role, type, name, id -- so that a
// message like {"role":"user","content":"..."} does not contribute the literal
// word "user" to the match surface.
func collectText(v any, sb *strings.Builder, depth int) {
	if depth > maxWalkDepth || sb.Len() >= maxPromptTextBytes {
		return
	}
	switch t := v.(type) {
	case string:
		if sb.Len() > 0 {
			sb.WriteByte('\n')
		}
		remaining := maxPromptTextBytes - sb.Len()
		if remaining <= 0 {
			return
		}
		if len(t) > remaining {
			t = t[:remaining]
		}
		sb.WriteString(t)
	case []any:
		for _, item := range t {
			collectText(item, sb, depth+1)
		}
	case map[string]any:
		// Iterate the known text-bearing keys in a FIXED order so the collected
		// text is deterministic; Go map iteration order is randomised and a
		// non-deterministic match surface would make guardrails flaky.
		for _, k := range []string{"content", "text", "prompt", "input", "value", "parts"} {
			if item, ok := t[k]; ok {
				collectText(item, sb, depth+1)
			}
		}
	}
}
