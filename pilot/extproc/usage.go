package main

import (
	"bytes"
	"encoding/json"
)

// Usage is the token accounting observed on a response.
type Usage struct {
	PromptTokens     int64
	CompletionTokens int64
	TotalTokens      int64
	Found            bool
}

// Total returns the authoritative total. Providers are inconsistent: some emit
// only the components, some only the total. Prefer the explicit total and
// reconstruct it when absent, so a provider that omits it does not silently
// produce delta = 0 - 100 = -100 and REFUND tokens the tenant actually spent.
func (u Usage) Total() int64 {
	if u.TotalTokens > 0 {
		return u.TotalTokens
	}
	return u.PromptTokens + u.CompletionTokens
}

// Delta is the TPM correction posted to /v1/usage: actual minus what was already
// pre-debited at /v1/check. Positive = the tenant owes more (the common case,
// since the flat estimate of 100 is nearly always too low). Negative = refund.
func Delta(total, estimated int64) int64 { return total - estimated }

// usageEnvelope matches both the non-streaming top-level object and the
// terminal SSE frame. `usage` is a POINTER on purpose: OpenAI-compatible
// streams emit `"usage": null` on every intermediate chunk, and decoding that
// into a value type would overwrite a real reading with zeros.
type usageEnvelope struct {
	Usage *struct {
		PromptTokens     int64 `json:"prompt_tokens"`
		CompletionTokens int64 `json:"completion_tokens"`
		TotalTokens      int64 `json:"total_tokens"`
	} `json:"usage"`
}

func parseUsageJSON(b []byte) (Usage, bool) {
	var env usageEnvelope
	if err := json.Unmarshal(b, &env); err != nil {
		return Usage{}, false
	}
	if env.Usage == nil {
		return Usage{}, false
	}
	u := Usage{
		PromptTokens:     env.Usage.PromptTokens,
		CompletionTokens: env.Usage.CompletionTokens,
		TotalTokens:      env.Usage.TotalTokens,
		Found:            true,
	}
	if u.Total() <= 0 {
		// A present-but-all-zero usage object carries no information; treating
		// it as found would post a -100 refund.
		return Usage{}, false
	}
	return u, true
}

var (
	dataPrefix = []byte("data:")
	usageToken = []byte("\"usage\"")
)

// SSEUsageScanner finds the token usage in a Server-Sent Events stream.
//
// THIS RUNS ON THE REALTIME PATH, after the chunk has already been forwarded.
// Its cost model is therefore part of the SSE guarantee:
//
//   - one copy of the chunk into a REUSED carry buffer (no per-chunk alloc once
//     the buffer has grown to steady state);
//   - one bytes.LastIndexByte over the buffer;
//   - one bytes.Contains for the literal `"usage"` -- a memchr-class scan that
//     rejects the ~99% of chunks that are ordinary content deltas before any
//     JSON decoding happens;
//   - JSON decoding ONLY for lines that already contain the token.
//
// It tolerates a frame split across any number of chunk boundaries by carrying
// the trailing partial line forward. The carry is bounded: a `data:` line longer
// than maxCarry is abandoned rather than allowed to grow, because a usage frame
// is a few hundred bytes and anything larger is not one.
type SSEUsageScanner struct {
	carry    []byte
	maxCarry int
	usage    Usage
	// Overflowed records that a line was dropped for exceeding maxCarry, so the
	// caller can distinguish "no usage in the stream" from "we stopped looking".
	Overflowed bool
}

func NewSSEUsageScanner(maxCarry int) *SSEUsageScanner {
	return &SSEUsageScanner{maxCarry: maxCarry}
}

// Feed consumes one response chunk. chunk is only READ; it is never retained.
func (s *SSEUsageScanner) Feed(chunk []byte) {
	if len(chunk) == 0 {
		return
	}
	s.carry = append(s.carry, chunk...)

	// Process only whole lines; keep the trailing partial for the next chunk.
	nl := bytes.LastIndexByte(s.carry, '\n')
	if nl < 0 {
		s.trim()
		return
	}
	complete := s.carry[:nl]
	if bytes.Contains(complete, usageToken) {
		s.scanLines(complete)
	}
	// Shift the remainder to the front, reusing the same backing array.
	n := copy(s.carry, s.carry[nl+1:])
	s.carry = s.carry[:n]
	s.trim()
}

func (s *SSEUsageScanner) trim() {
	if len(s.carry) > s.maxCarry {
		s.carry = s.carry[:0]
		s.Overflowed = true
	}
}

func (s *SSEUsageScanner) scanLines(block []byte) {
	for len(block) > 0 {
		var line []byte
		if i := bytes.IndexByte(block, '\n'); i >= 0 {
			line, block = block[:i], block[i+1:]
		} else {
			line, block = block, nil
		}
		line = bytes.TrimRight(line, "\r")
		if !bytes.HasPrefix(line, dataPrefix) {
			continue
		}
		payload := bytes.TrimSpace(line[len(dataPrefix):])
		if len(payload) == 0 || bytes.Equal(payload, []byte("[DONE]")) {
			continue
		}
		if !bytes.Contains(payload, usageToken) {
			continue
		}
		if u, ok := parseUsageJSON(payload); ok {
			// Last writer wins: if a provider emits usage more than once, the
			// terminal frame is the authoritative one.
			s.usage = u
		}
	}
}

// Result returns the usage observed so far.
func (s *SSEUsageScanner) Result() Usage { return s.usage }

// BodyAccumulator collects a NON-STREAMING response body so the top-level
// `usage` object can be read at end of stream. Unlike the SSE path this must
// hold bytes -- a JSON object cannot be parsed incrementally without a streaming
// parser -- but it is bounded and, critically, it still never delays a chunk:
// the chunk is forwarded to the gateway BEFORE it is appended here.
type BodyAccumulator struct {
	buf      []byte
	max      int
	Overflow bool
}

func NewBodyAccumulator(max int) *BodyAccumulator { return &BodyAccumulator{max: max} }

func (a *BodyAccumulator) Feed(chunk []byte) {
	if a.Overflow || len(chunk) == 0 {
		return
	}
	if len(a.buf)+len(chunk) > a.max {
		// Stop collecting and release what we have: a truncated JSON document
		// is useless, so holding it would be pure memory cost.
		a.Overflow = true
		a.buf = nil
		return
	}
	a.buf = append(a.buf, chunk...)
}

func (a *BodyAccumulator) Result() Usage {
	if a.Overflow || len(a.buf) == 0 {
		return Usage{}
	}
	if !bytes.Contains(a.buf, usageToken) {
		return Usage{}
	}
	u, _ := parseUsageJSON(a.buf)
	return u
}

func (a *BodyAccumulator) Bytes() []byte { return a.buf }
