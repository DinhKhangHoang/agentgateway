package main

import (
	"strings"
	"testing"
)

func TestExtractRequestFacts(t *testing.T) {
	cases := []struct {
		name       string
		body       string
		wantModel  string
		wantInText []string
		notInText  []string
	}{
		{
			name:       "chat with a string content",
			body:       `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hello world"}]}`,
			wantModel:  "gemini-2.5-flash",
			wantInText: []string{"hello world"},
			// The structural role must not become part of the match surface, or
			// a keyword of "user" or "system" would fire on every request.
			notInText: []string{"gemini-2.5-flash", "user"},
		},
		{
			name: "chat with multi-part content",
			body: `{"model":"gpt-4o","messages":[{"role":"user","content":[` +
				`{"type":"text","text":"first part"},{"type":"text","text":"second part"}]}]}`,
			wantModel:  "gpt-4o",
			wantInText: []string{"first part", "second part"},
			notInText:  []string{"text"},
		},
		{
			name:       "multiple messages are all scanned",
			body:       `{"model":"m","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"secretword"}]}`,
			wantModel:  "m",
			wantInText: []string{"be terse", "secretword"},
		},
		{
			name:       "legacy completion prompt as a string",
			body:       `{"model":"deepseek-v4-pro","prompt":"complete this sentence"}`,
			wantModel:  "deepseek-v4-pro",
			wantInText: []string{"complete this sentence"},
		},
		{
			name:       "legacy completion prompt as an array",
			body:       `{"model":"m","prompt":["one","two"]}`,
			wantModel:  "m",
			wantInText: []string{"one", "two"},
		},
		{
			name:       "responses-style input string",
			body:       `{"model":"m","input":"just this"}`,
			wantModel:  "m",
			wantInText: []string{"just this"},
		},
		{
			name:       "responses-style input array of objects",
			body:       `{"model":"m","input":[{"role":"user","content":[{"type":"input_text","text":"deep text"}]}]}`,
			wantModel:  "m",
			wantInText: []string{"deep text"},
		},
		{
			name:       "embeddings-style input array",
			body:       `{"model":"m","input":["alpha","beta"]}`,
			wantModel:  "m",
			wantInText: []string{"alpha", "beta"},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := ExtractRequestFacts([]byte(tc.body))
			if f.Model != tc.wantModel {
				t.Fatalf("model = %q, want %q", f.Model, tc.wantModel)
			}
			for _, want := range tc.wantInText {
				if !strings.Contains(f.Prompt, want) {
					t.Fatalf("prompt %q is missing %q", f.Prompt, want)
				}
			}
			for _, no := range tc.notInText {
				if strings.Contains(f.Prompt, no) {
					t.Fatalf("prompt %q must not contain %q", f.Prompt, no)
				}
			}
		})
	}
}

func TestExtractRequestFactsStreamFlag(t *testing.T) {
	if !ExtractRequestFacts([]byte(`{"model":"m","stream":true}`)).Stream {
		t.Fatal("stream true not detected")
	}
	if ExtractRequestFacts([]byte(`{"model":"m"}`)).Stream {
		t.Fatal("stream must default to false")
	}
}

// The model string is the join key for BOTH the guardrail map and the per-model
// TPM counters. Any normalisation here silently splits a tenant's counters.
func TestModelIsNotNormalised(t *testing.T) {
	f := ExtractRequestFacts([]byte(`{"model":"  DeepSeek-V4-Pro  ","messages":[]}`))
	if f.Model != "  DeepSeek-V4-Pro  " {
		t.Fatalf("model = %q; it must be forwarded byte-identical", f.Model)
	}
}

func TestNonJSONBodyIsNotAnError(t *testing.T) {
	f := ExtractRequestFacts([]byte("this is not json at all"))
	if f.Model != "" || f.Prompt != "" {
		t.Fatalf("expected a zero value, got %+v", f)
	}
}

func TestEmptyBody(t *testing.T) {
	f := ExtractRequestFacts(nil)
	if f.Model != "" || f.Prompt != "" {
		t.Fatalf("expected a zero value, got %+v", f)
	}
}

func TestPromptTextIsBounded(t *testing.T) {
	huge := strings.Repeat("x", 4<<20)
	f := ExtractRequestFacts([]byte(`{"model":"m","prompt":"` + huge + `"}`))
	if len(f.Prompt) > maxPromptTextBytes+16 {
		t.Fatalf("prompt text grew to %d, cap is %d", len(f.Prompt), maxPromptTextBytes)
	}
}

func TestDeeplyNestedBodyTerminates(t *testing.T) {
	body := `{"model":"m","input":`
	closing := ""
	for i := 0; i < 200; i++ {
		body += `[{"content":`
		closing += `}]`
	}
	body += `"needle"` + closing + `}`
	// The assertion is simply that this returns rather than blowing the stack.
	_ = ExtractRequestFacts([]byte(body))
}
