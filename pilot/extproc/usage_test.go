package main

import (
	"fmt"
	"strings"
	"testing"
)

func TestUsageFromNonStreamingBody(t *testing.T) {
	body := `{"id":"chatcmpl-1","object":"chat.completion",` +
		`"choices":[{"index":0,"message":{"role":"assistant","content":"Paris."}}],` +
		`"usage":{"prompt_tokens":18,"completion_tokens":124,"total_tokens":142}}`

	acc := NewBodyAccumulator(1 << 20)
	// Feed in three arbitrary pieces: a non-streaming body still arrives as
	// multiple chunks over the wire.
	acc.Feed([]byte(body[:40]))
	acc.Feed([]byte(body[40:90]))
	acc.Feed([]byte(body[90:]))

	u := acc.Result()
	if !u.Found {
		t.Fatal("usage not found in a non-streaming body")
	}
	if u.PromptTokens != 18 || u.CompletionTokens != 124 || u.TotalTokens != 142 {
		t.Fatalf("got %+v", u)
	}
	if u.Total() != 142 {
		t.Fatalf("Total() = %d", u.Total())
	}
}

func TestBodyAccumulatorOverflowAbandonsRatherThanGrows(t *testing.T) {
	acc := NewBodyAccumulator(64)
	acc.Feed([]byte(strings.Repeat("x", 40)))
	acc.Feed([]byte(strings.Repeat("x", 40)))
	if !acc.Overflow {
		t.Fatal("expected overflow")
	}
	if len(acc.Bytes()) != 0 {
		t.Fatal("overflowed accumulator must release its buffer")
	}
	if acc.Result().Found {
		t.Fatal("an overflowed scan must not claim a usage reading")
	}
}

// deepseek's real terminal frame shape, verified live.
const deepseekTerminal = `data: {"id":"x","choices":[],"usage":{"prompt_tokens":18,"completion_tokens":124,"total_tokens":142,"prompt_cache_hit_tokens":0}}` + "\n\n"

func TestUsageFromTerminalSSEFrame(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte("data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}],\"usage\":null}\n\n"))
	s.Feed([]byte("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":null}\n\n"))
	s.Feed([]byte(deepseekTerminal))
	s.Feed([]byte("data: [DONE]\n\n"))

	u := s.Result()
	if !u.Found {
		t.Fatal("usage not found in the terminal SSE frame")
	}
	if u.PromptTokens != 18 || u.CompletionTokens != 124 || u.TotalTokens != 142 {
		t.Fatalf("got %+v", u)
	}
}

// `"usage": null` appears on EVERY intermediate OpenAI-compatible chunk. If it
// were decoded into a value type it would zero out a real reading.
func TestNullUsageFramesAreIgnored(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte(deepseekTerminal))
	s.Feed([]byte("data: {\"choices\":[],\"usage\":null}\n\n"))
	if got := s.Result().TotalTokens; got != 142 {
		t.Fatalf("a trailing null usage clobbered the reading: total = %d", got)
	}
}

func TestUsageFromSSEFrameSplitAcrossChunks(t *testing.T) {
	// Split the terminal frame at every single byte offset. Any off-by-one in
	// the carry logic shows up as a miss at exactly one split point.
	for cut := 1; cut < len(deepseekTerminal); cut++ {
		s := NewSSEUsageScanner(64 << 10)
		s.Feed([]byte("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"))
		s.Feed([]byte(deepseekTerminal[:cut]))
		s.Feed([]byte(deepseekTerminal[cut:]))
		u := s.Result()
		if !u.Found || u.TotalTokens != 142 {
			t.Fatalf("split at %d lost the usage frame: %+v", cut, u)
		}
	}
}

func TestUsageFromSSEFrameSplitIntoManyTinyChunks(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	for i := 0; i < len(deepseekTerminal); i += 3 {
		end := i + 3
		if end > len(deepseekTerminal) {
			end = len(deepseekTerminal)
		}
		s.Feed([]byte(deepseekTerminal[i:end]))
	}
	if u := s.Result(); !u.Found || u.TotalTokens != 142 {
		t.Fatalf("byte-dribbled frame lost: %+v", u)
	}
}

func TestSSEScannerCarryIsBounded(t *testing.T) {
	s := NewSSEUsageScanner(256)
	// A single line with no newline, far larger than the cap.
	s.Feed([]byte("data: " + strings.Repeat("A", 4096)))
	if len(s.carry) > 256 {
		t.Fatalf("carry grew to %d, cap is 256", len(s.carry))
	}
	if !s.Overflowed {
		t.Fatal("overflow must be recorded so a missing true-up is explainable")
	}
}

func TestGeminiStyleUsageWithoutExplicitTotal(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte(`data: {"usage":{"prompt_tokens":10,"completion_tokens":90}}` + "\n\n"))
	u := s.Result()
	if !u.Found {
		t.Fatal("usage with only components must still be found")
	}
	if u.Total() != 100 {
		t.Fatalf("Total() = %d, want 100 reconstructed from the components", u.Total())
	}
}

func TestAllZeroUsageIsNotTreatedAsFound(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte(`data: {"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}` + "\n\n"))
	if s.Result().Found {
		t.Fatal("an all-zero usage object carries no information and would post a -100 refund")
	}
}

func TestDeltaArithmetic(t *testing.T) {
	cases := []struct {
		name      string
		total     int64
		estimated int64
		want      int64
	}{
		{"under-estimate is the common case", 142, 100, 42},
		{"exactly the estimate short-circuits", 100, 100, 0},
		{"over-estimate refunds", 40, 100, -60},
		{"large stream", 12000, 100, 11900},
		{"zero estimate passes the total through", 142, 0, 142},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := Delta(tc.total, tc.estimated); got != tc.want {
				t.Fatalf("Delta(%d,%d) = %d, want %d", tc.total, tc.estimated, got, tc.want)
			}
		})
	}
}

func TestSSEScannerHandlesCRLFAndNoTrailingNewline(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte("data: {\"usage\":{\"total_tokens\":7}}\r\n\r\n"))
	if u := s.Result(); !u.Found || u.TotalTokens != 7 {
		t.Fatalf("CRLF frame not parsed: %+v", u)
	}
}

func TestNonDataLinesAreIgnored(t *testing.T) {
	s := NewSSEUsageScanner(64 << 10)
	s.Feed([]byte(": comment containing the word usage\n"))
	s.Feed([]byte("event: message\n"))
	s.Feed([]byte(fmt.Sprintf("id: %d\n", 1)))
	if s.Result().Found {
		t.Fatal("non-data lines must not produce a usage reading")
	}
}
