package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode"
)

// GuardrailDirective mirrors contract::GuardrailDirective in
// ai-gateway-plugin-server (crates/contract/src/lib.rs:51). Wire shape is
// {type, plugin, config}; `config` is opaque to the plugin server and is
// interpreted HERE.
type GuardrailDirective struct {
	Kind   string          `json:"type"`
	Plugin string          `json:"plugin"`
	Config json.RawMessage `json:"config"`
}

// keywordConfig is the pilot's interpretation of a `keyword` directive's opaque
// config blob.
//
// PRODUCTION KONG DOES SOMETHING DIFFERENT and that difference is deliberate.
// The real keyword-guard-request plugin
// (/home/stackops/kong-plugin-guardrail/kong/plugins/keyword-guard-request/schema.lua)
// carries `external_keyword_service_url` and POSTs each message to a separate
// keyword microservice. That service does not exist in the pilot namespace and
// cannot be stood up without inventing its scoring semantics, so reproducing it
// would prove nothing.
//
// Instead the pilot enforces an INLINE keyword list, which is deterministic and
// therefore actually testable -- the point of the exercise is proving that a
// per-tenant directive fetched at runtime changes enforcement, not
// re-implementing a third-party scorer. A directive carrying only
// `external_keyword_service_url` and no inline list is treated as UNSUPPORTED
// and follows UNSUPPORTED_GUARDRAIL_ACTION (deny by default) rather than being
// silently passed -- see unsupportedReason below.
type keywordConfig struct {
	Keywords        []string `json:"keywords"`
	BlockedKeywords []string `json:"blocked_keywords"`
	DenyList        []string `json:"deny_list"`

	CaseSensitive bool `json:"case_sensitive"`
	// MatchMode is "substring" (default) or "word". "word" requires
	// non-alphanumeric boundaries on both sides, so "class" does not trip a
	// keyword of "ass".
	MatchMode string `json:"match"`

	// Message and Code override the client-visible error, so an operator can
	// explain a tenant-specific policy without a code change.
	Message string `json:"message"`
	Code    string `json:"code"`
}

func (k keywordConfig) terms() []string {
	out := make([]string, 0, len(k.Keywords)+len(k.BlockedKeywords)+len(k.DenyList))
	for _, group := range [][]string{k.Keywords, k.BlockedKeywords, k.DenyList} {
		for _, t := range group {
			if t = strings.TrimSpace(t); t != "" {
				out = append(out, t)
			}
		}
	}
	return out
}

// Violation is a decision to reject. It carries everything needed to build the
// ImmediateResponse and nothing that could leak prompt content: Term is the
// OPERATOR-configured keyword, never the surrounding user text.
type Violation struct {
	StatusCode int
	Code       string
	Message    string
	// Kind is the directive type that produced this, for logs/metrics.
	Kind string
	// Term is the configured keyword that matched. Safe to log: it is
	// operator-supplied configuration, not user input.
	Term string
}

// Body renders the client-visible JSON. It mirrors the envelope the pilot's
// existing STATIC guardrail returns (pilot/04-model-gemini.yaml promptGuard
// `response.message`) verbatim in shape, so a client sees ONE contract whether
// it was blocked by the static regex or by a per-tenant directive.
// agentgateway returns an ImmediateResponse body unwrapped and without setting
// a content-type, so the full OpenAI error envelope is spelled out here.
func (v Violation) Body() string {
	b, err := json.Marshal(map[string]any{
		"error": map[string]any{
			"message": v.Message,
			"type":    "invalid_request_error",
			"param":   nil,
			"code":    v.Code,
		},
	})
	if err != nil {
		// Unreachable for this fixed shape; a plain deny is still the right
		// fail-closed outcome.
		return `{"error":{"message":"Blocked by gateway guardrail.","type":"invalid_request_error","param":null,"code":"guardrail_error"}}`
	}
	return string(b)
}

// guardrailLogger is the narrow logging surface the evaluator needs. Injected so
// the "skip must log loudly" requirement is assertable in a test rather than
// taken on faith.
type guardrailLogger interface {
	Warn(msg string, args ...any)
}

// EvaluateGuardrails applies the directive list in order and returns the first
// violation, or nil to continue.
//
// Ordering is significant and preserved verbatim: the backend emits each model's
// list sorted by Kong plugin priority (keyword 800 -> prompt 790 -> presidio 780
// -> llama) and this never re-sorts.
func EvaluateGuardrails(directives []GuardrailDirective, prompt string, action UnsupportedAction, log guardrailLogger) *Violation {
	for i, d := range directives {
		kind := strings.ToLower(strings.TrimSpace(d.Kind))
		switch kind {
		case "keyword":
			v, reason := evaluateKeyword(d, prompt)
			if v != nil {
				return v
			}
			if reason != "" {
				if u := handleUnsupported(kind, d.Plugin, i, reason, action, log); u != nil {
					return u
				}
			}
		default:
			// prompt / presidio / llama all delegate to external scoring
			// services that do not exist in the pilot. There is no honest way
			// to enforce them here.
			reason := fmt.Sprintf("directive kind %q requires an external scoring service that this pilot does not run", kind)
			if u := handleUnsupported(kind, d.Plugin, i, reason, action, log); u != nil {
				return u
			}
		}
	}
	return nil
}

// handleUnsupported implements the deny-by-default policy. A skip is a
// deliberate, LOUD downgrade of enforcement, so it is logged at WARN with the
// directive kind, the plugin name and the reason -- never with prompt content.
func handleUnsupported(kind, plugin string, index int, reason string, action UnsupportedAction, log guardrailLogger) *Violation {
	if action == UnsupportedSkip {
		if log != nil {
			log.Warn("SKIPPING an unsupported guardrail directive: enforcement is DISABLED for this directive because UNSUPPORTED_GUARDRAIL_ACTION=skip",
				"guardrail_kind", kind,
				"guardrail_plugin", plugin,
				"directive_index", index,
				"reason", reason)
		}
		return nil
	}
	return &Violation{
		StatusCode: 403,
		Code:       "guardrail_unsupported",
		Kind:       kind,
		Message: "Blocked by gateway guardrail: a guardrail of type \"" + kind +
			"\" is configured for this key and model but is not supported by this gateway, so the request cannot be safely allowed.",
	}
}

// evaluateKeyword returns (violation, unsupportedReason). Exactly one is
// meaningful: a non-empty reason means the directive could not be enforced.
func evaluateKeyword(d GuardrailDirective, prompt string) (*Violation, string) {
	var cfg keywordConfig
	if len(d.Config) > 0 {
		if err := json.Unmarshal(d.Config, &cfg); err != nil {
			// A config we cannot parse is a guardrail we cannot enforce.
			return nil, "keyword directive config is not a JSON object this gateway understands"
		}
	}
	terms := cfg.terms()
	if len(terms) == 0 {
		return nil, "keyword directive carries no inline keyword list (this pilot does not run the external keyword service Kong delegates to)"
	}

	hay := prompt
	if !cfg.CaseSensitive {
		hay = strings.ToLower(hay)
	}
	wordMode := strings.EqualFold(strings.TrimSpace(cfg.MatchMode), "word")

	for _, term := range terms {
		needle := term
		if !cfg.CaseSensitive {
			needle = strings.ToLower(needle)
		}
		if !matches(hay, needle, wordMode) {
			continue
		}
		msg := cfg.Message
		if msg == "" {
			msg = "Blocked by gateway guardrail: the prompt contains a term this key is not permitted to use."
		}
		code := cfg.Code
		if code == "" {
			code = "guardrail_keyword"
		}
		return &Violation{
			StatusCode: 403,
			Code:       code,
			Message:    msg,
			Kind:       "keyword",
			Term:       term,
		}, ""
	}
	return nil, ""
}

func matches(hay, needle string, wordMode bool) bool {
	if needle == "" {
		return false
	}
	if !wordMode {
		return strings.Contains(hay, needle)
	}
	from := 0
	for {
		i := strings.Index(hay[from:], needle)
		if i < 0 {
			return false
		}
		start := from + i
		end := start + len(needle)
		if isBoundary(hay, start-1) && isBoundary(hay, end) {
			return true
		}
		from = start + 1
		if from >= len(hay) {
			return false
		}
	}
}

func isBoundary(s string, i int) bool {
	if i < 0 || i >= len(s) {
		return true
	}
	r := rune(s[i])
	return !unicode.IsLetter(r) && !unicode.IsDigit(r)
}
