package main

import (
	"encoding/json"
	"strings"
	"testing"
)

type recordingLogger struct {
	warns []string
	args  [][]any
}

func (r *recordingLogger) Warn(msg string, args ...any) {
	r.warns = append(r.warns, msg)
	r.args = append(r.args, args)
}

func kwDirective(t *testing.T, cfg map[string]any) GuardrailDirective {
	t.Helper()
	b, err := json.Marshal(cfg)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return GuardrailDirective{Kind: "keyword", Plugin: "keyword-guard-request", Config: b}
}

func TestKeywordGuardrail(t *testing.T) {
	cases := []struct {
		name      string
		cfg       map[string]any
		prompt    string
		wantBlock bool
		wantTerm  string
	}{
		{
			name:      "hit blocks",
			cfg:       map[string]any{"keywords": []string{"vng-secret-project"}},
			prompt:    "tell me about the vng-secret-project roadmap",
			wantBlock: true,
			wantTerm:  "vng-secret-project",
		},
		{
			name:      "miss continues",
			cfg:       map[string]any{"keywords": []string{"vng-secret-project"}},
			prompt:    "what is the capital of France?",
			wantBlock: false,
		},
		{
			name:      "case insensitive by default",
			cfg:       map[string]any{"keywords": []string{"Confidential"}},
			prompt:    "this is CONFIDENTIAL material",
			wantBlock: true,
			wantTerm:  "Confidential",
		},
		{
			name:      "case sensitive honours the flag",
			cfg:       map[string]any{"keywords": []string{"Confidential"}, "case_sensitive": true},
			prompt:    "this is confidential material",
			wantBlock: false,
		},
		{
			name:      "word mode does not fire on a substring",
			cfg:       map[string]any{"keywords": []string{"ass"}, "match": "word"},
			prompt:    "the class assembled",
			wantBlock: false,
		},
		{
			name:      "word mode fires on a whole word",
			cfg:       map[string]any{"keywords": []string{"nuke"}, "match": "word"},
			prompt:    "how do I nuke the cluster",
			wantBlock: true,
			wantTerm:  "nuke",
		},
		{
			name:      "blocked_keywords alias is honoured",
			cfg:       map[string]any{"blocked_keywords": []string{"forbidden"}},
			prompt:    "a forbidden request",
			wantBlock: true,
			wantTerm:  "forbidden",
		},
		{
			name:      "deny_list alias is honoured",
			cfg:       map[string]any{"deny_list": []string{"blocklisted"}},
			prompt:    "a blocklisted request",
			wantBlock: true,
			wantTerm:  "blocklisted",
		},
		{
			name:      "first matching term wins across a list",
			cfg:       map[string]any{"keywords": []string{"alpha", "beta"}},
			prompt:    "only beta appears here",
			wantBlock: true,
			wantTerm:  "beta",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			lg := &recordingLogger{}
			v := EvaluateGuardrails([]GuardrailDirective{kwDirective(t, tc.cfg)}, tc.prompt, UnsupportedDeny, lg)
			if tc.wantBlock {
				if v == nil {
					t.Fatalf("expected a block, got allow")
				}
				if v.StatusCode != 403 {
					t.Fatalf("status = %d, want 403", v.StatusCode)
				}
				if v.Term != tc.wantTerm {
					t.Fatalf("term = %q, want %q", v.Term, tc.wantTerm)
				}
				if v.Code != "guardrail_keyword" {
					t.Fatalf("code = %q, want guardrail_keyword", v.Code)
				}
			} else if v != nil {
				t.Fatalf("expected allow, got block %+v", v)
			}
		})
	}
}

// The 403 body must be the SAME envelope the pilot's static promptGuard returns
// (pilot/04-model-gemini.yaml) so clients see one contract.
func TestViolationBodyMatchesStaticGuardrailEnvelope(t *testing.T) {
	v := Violation{StatusCode: 403, Code: "guardrail_keyword", Message: "Blocked by gateway guardrail: nope."}
	var doc struct {
		Error struct {
			Message string  `json:"message"`
			Type    string  `json:"type"`
			Param   *string `json:"param"`
			Code    string  `json:"code"`
		} `json:"error"`
	}
	if err := json.Unmarshal([]byte(v.Body()), &doc); err != nil {
		t.Fatalf("body is not valid JSON: %v", err)
	}
	if doc.Error.Type != "invalid_request_error" {
		t.Fatalf("type = %q", doc.Error.Type)
	}
	if doc.Error.Param != nil {
		t.Fatalf("param should be null")
	}
	if doc.Error.Code != "guardrail_keyword" || doc.Error.Message != v.Message {
		t.Fatalf("unexpected body: %s", v.Body())
	}
}

func TestUnsupportedGuardrailAction(t *testing.T) {
	unsupported := []GuardrailDirective{
		{Kind: "presidio", Plugin: "presidio-ai-guard-request", Config: json.RawMessage(`{"external_presidio_service_url":"http://x"}`)},
	}

	t.Run("default from empty env is deny", func(t *testing.T) {
		cfg, err := LoadConfig(func(k string) string {
			if k == "POLICY_SERVER_URL" {
				return "http://policy:8080"
			}
			return ""
		})
		if err != nil {
			t.Fatalf("LoadConfig: %v", err)
		}
		if cfg.UnsupportedGuardrailAction != UnsupportedDeny {
			t.Fatalf("default action = %q, want deny", cfg.UnsupportedGuardrailAction)
		}
	})

	t.Run("deny blocks", func(t *testing.T) {
		lg := &recordingLogger{}
		v := EvaluateGuardrails(unsupported, "harmless prompt", UnsupportedDeny, lg)
		if v == nil {
			t.Fatal("expected deny for an unsupported directive")
		}
		if v.Code != "guardrail_unsupported" || v.StatusCode != 403 {
			t.Fatalf("got %+v", v)
		}
		if !strings.Contains(v.Message, "presidio") {
			t.Fatalf("message should name the kind: %q", v.Message)
		}
	})

	t.Run("skip continues and logs loudly with the kind", func(t *testing.T) {
		lg := &recordingLogger{}
		v := EvaluateGuardrails(unsupported, "harmless prompt", UnsupportedSkip, lg)
		if v != nil {
			t.Fatalf("expected allow under skip, got %+v", v)
		}
		if len(lg.warns) != 1 {
			t.Fatalf("expected exactly one WARN, got %d", len(lg.warns))
		}
		flat := lg.warns[0] + " " + flattenArgs(lg.args[0])
		if !strings.Contains(flat, "presidio") {
			t.Fatalf("WARN must carry the directive kind, got: %s", flat)
		}
		if !strings.Contains(strings.ToUpper(flat), "SKIP") {
			t.Fatalf("WARN must be unmistakable, got: %s", flat)
		}
	})

	t.Run("a keyword directive with no inline list is unsupported not silently allowed", func(t *testing.T) {
		// This is the REAL Kong shape: enforcement is delegated to an external
		// keyword service the pilot does not run. Passing it through would be a
		// silent loss of enforcement.
		d := []GuardrailDirective{kwDirective(t, map[string]any{
			"external_keyword_service_url": "http://kw-svc.internal/check",
			"max_parallel_check_requests":  5,
		})}
		if v := EvaluateGuardrails(d, "anything", UnsupportedDeny, &recordingLogger{}); v == nil {
			t.Fatal("a keyword directive we cannot enforce must not be silently allowed")
		}
		lg := &recordingLogger{}
		if v := EvaluateGuardrails(d, "anything", UnsupportedSkip, lg); v != nil {
			t.Fatalf("skip should allow, got %+v", v)
		}
		if len(lg.warns) != 1 {
			t.Fatalf("skip must still log, got %d warns", len(lg.warns))
		}
	})

	t.Run("unrecognised action value is rejected at startup", func(t *testing.T) {
		_, err := LoadConfig(func(k string) string {
			switch k {
			case "POLICY_SERVER_URL":
				return "http://policy:8080"
			case "UNSUPPORTED_GUARDRAIL_ACTION":
				return "allow"
			}
			return ""
		})
		if err == nil {
			t.Fatal("a typo must crash the process, not silently disable enforcement")
		}
	})
}

// Directive order is significant (keyword 800 -> prompt 790 -> presidio 780 ->
// llama) and must be preserved verbatim.
func TestDirectiveOrderIsPreserved(t *testing.T) {
	ds := []GuardrailDirective{
		kwDirective(t, map[string]any{"keywords": []string{"tripwire"}}),
		{Kind: "llama", Plugin: "llama-ai-guard-request", Config: json.RawMessage(`{}`)},
	}
	v := EvaluateGuardrails(ds, "this contains a tripwire", UnsupportedDeny, &recordingLogger{})
	if v == nil || v.Kind != "keyword" {
		t.Fatalf("the earlier keyword directive must win, got %+v", v)
	}
}

func TestEmptyDirectiveListAllows(t *testing.T) {
	if v := EvaluateGuardrails(nil, "anything at all", UnsupportedDeny, &recordingLogger{}); v != nil {
		t.Fatalf("no directives must mean no enforcement, got %+v", v)
	}
}

func flattenArgs(args []any) string {
	var sb strings.Builder
	for _, a := range args {
		sb.WriteString(strings.TrimSpace(toStr(a)))
		sb.WriteByte(' ')
	}
	return sb.String()
}

func toStr(a any) string {
	switch v := a.(type) {
	case string:
		return v
	default:
		b, _ := json.Marshal(v)
		return string(b)
	}
}
