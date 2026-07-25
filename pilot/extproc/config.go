// Command extproc is the pilot's ext_proc service. It closes two gaps between
// the agentgateway pilot and production Kong with ONE processor:
//
//	Gap 1 (per-tenant guardrails): Kong injects a per-tenant, per-model
//	  guardrail directive list at runtime. The pilot previously had only a
//	  STATIC per-model regex (pilot/04-model-gemini.yaml promptGuard). This
//	  service fetches KeyConfig.guardrails[<model>] from the plugin server at
//	  request time and enforces it.
//
//	Gap 2 (TPM true-up): Kong pre-debits estimated_tokens=100 at /v1/check and
//	  corrects it at /v1/usage in its log phase. The pilot never corrected, so
//	  every tenant was under-debited by (actual - 100) tokens per request. This
//	  service observes the real usage and posts the delta.
//
// # THE ONE PROPERTY THAT MATTERS: SSE MUST STAY REALTIME
//
// agentgateway hardcodes observability_mode=false (7 places in
// crates/agentgateway/src/http/ext_proc.rs), so the gateway ALWAYS waits for
// our response to every message; there is no fire-and-forget mode to opt into.
// In FullDuplexStreamed the upstream body is replaced by a channel-fed
// StreamBody (http/ext_proc/buffering.rs:342-355), so EVERY response chunk
// round-trips gateway -> this process -> gateway before reaching the client.
// Our per-chunk handling is literally inside the user's token stream.
//
// The rule that follows, enforced by processor.go and by
// TestResponseBodyChunkIsForwardedWithoutWaitingForMoreChunks:
//
//	On a response body chunk we Send() the echo FIRST, before any parsing,
//	locking, allocation-heavy work, or I/O. Nothing is ever held back waiting
//	for a later chunk. The usage scan runs AFTER the Send, is bounded, and
//	reuses its buffers. /v1/usage is fire-and-forget on its own goroutine.
//
// # WHY THE REQUEST HEADERS ACK IS DEFERRED
//
// Counter-intuitively we do NOT ack request_headers immediately. Because
// requestBodyMode is FullDuplexStreamed, agentgateway streams body chunks to us
// WITHOUT waiting for the headers response (ext_proc.rs:894-917), so deferring
// costs no latency. What it buys is large: an ImmediateResponse sent as our
// FIRST message is caught by the main request loop (ext_proc.rs:993) and the
// upstream provider is NEVER contacted. If we ack the headers first,
// mutate_request returns, the backend call is dispatched, and a later deny is
// only recovered via the deferred request_body_immediate_response path
// (httpproxy.rs:1355) -- correct for the client, but the provider was already
// dialed. Matching promptGuard's "the provider is never called" behaviour is
// worth the deferral. See DEFER_REQUEST_HEADERS_ACK for the escape hatch and
// processor.go for the two conditions that force an immediate ack anyway.
package main

import (
	"fmt"
	"strconv"
	"strings"
	"time"
)

// UnsupportedAction decides what happens when a tenant has a guardrail
// directive configured that this processor cannot enforce.
type UnsupportedAction string

const (
	// UnsupportedDeny rejects the request. THE DEFAULT: silently ignoring a
	// guardrail the tenant paid for and an operator configured is a security
	// failure, not a graceful degradation.
	UnsupportedDeny UnsupportedAction = "deny"
	// UnsupportedSkip lets the request through, logging loudly at WARN with
	// the directive kind so the gap is visible in the logs rather than silent.
	UnsupportedSkip UnsupportedAction = "skip"
)

// Config is the fully validated runtime configuration.
type Config struct {
	// PolicyURL is the ai-gateway-plugin-server base URL (no trailing slash).
	PolicyURL string
	// GRPCPort serves ExternalProcessor/Process.
	GRPCPort string
	// HealthPort serves /healthz and /metrics on plain HTTP for kubelet probes.
	// Separate from gRPC so a probe can never be confused with a real stream.
	HealthPort string

	// EstimatedTokens MUST byte-match the estimated_tokens the gateway sends to
	// /v1/check (pilot/08-policy-extauth.yaml sends 100, matching Kong). The
	// true-up is delta = actual_total - EstimatedTokens; a mismatch here
	// silently corrupts every tenant's TPM counter.
	EstimatedTokens int64

	UnsupportedGuardrailAction UnsupportedAction

	// CheckTimeout bounds the /v1/check call made during the request phase. It
	// is on the request critical path (the response has not started), so it may
	// be generous; it is NOT on the SSE path.
	CheckTimeout time.Duration
	// UsageTimeout bounds the fire-and-forget /v1/usage call. Kong uses
	// usage_timeout_ms: 2000; matched here so the two behave the same under a
	// slow plugin server.
	UsageTimeout time.Duration

	// MaxRequestBodyBytes caps request-body accumulation. Buffering the REQUEST
	// body is safe (it is not the user-visible realtime stream and Kong buffers
	// too), but it must be bounded or a large upload is an OOM.
	MaxRequestBodyBytes int
	// MaxUsageScanBytes caps NON-STREAMING response accumulation used to find
	// the top-level `usage` object. Exceeding it abandons the true-up for that
	// request (logged) rather than growing without limit.
	MaxUsageScanBytes int
	// MaxSSECarryBytes caps the partial-line carry used to stitch SSE frames
	// split across chunk boundaries. A single `data:` line longer than this is
	// abandoned; real usage frames are a few hundred bytes.
	MaxSSECarryBytes int

	// DeferHeadersAck controls the deferral described in the package comment.
	// Escape hatch: set false to ack request_headers immediately if the
	// deferral ever interacts badly with a future gateway version.
	DeferHeadersAck bool
}

func envStr(get func(string) string, name, def string) string {
	if v := strings.TrimSpace(get(name)); v != "" {
		return v
	}
	return def
}

func envInt(get func(string) string, name string, def int) (int, error) {
	v := strings.TrimSpace(get(name))
	if v == "" {
		return def, nil
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return 0, fmt.Errorf("%s: %q is not an integer", name, v)
	}
	return n, nil
}

func envBool(get func(string) string, name string, def bool) (bool, error) {
	v := strings.TrimSpace(get(name))
	if v == "" {
		return def, nil
	}
	b, err := strconv.ParseBool(v)
	if err != nil {
		return false, fmt.Errorf("%s: %q is not a boolean", name, v)
	}
	return b, nil
}

// LoadConfig validates the environment. getenv is injected so this is testable
// without mutating process state.
func LoadConfig(getenv func(string) string) (*Config, error) {
	policy := strings.TrimRight(strings.TrimSpace(getenv("POLICY_SERVER_URL")), "/")
	if policy == "" {
		return nil, fmt.Errorf("POLICY_SERVER_URL is required")
	}

	// Fail-closed by DEFAULT and on any unrecognised value. An operator typo
	// must never silently disable guardrail enforcement.
	action := UnsupportedDeny
	switch strings.ToLower(envStr(getenv, "UNSUPPORTED_GUARDRAIL_ACTION", "deny")) {
	case "deny":
		action = UnsupportedDeny
	case "skip":
		action = UnsupportedSkip
	default:
		return nil, fmt.Errorf("UNSUPPORTED_GUARDRAIL_ACTION must be \"deny\" or \"skip\"")
	}

	est, err := envInt(getenv, "ESTIMATED_TOKENS", 100)
	if err != nil {
		return nil, err
	}
	if est < 0 {
		return nil, fmt.Errorf("ESTIMATED_TOKENS must not be negative")
	}
	checkMS, err := envInt(getenv, "CHECK_TIMEOUT_MS", 3000)
	if err != nil {
		return nil, err
	}
	usageMS, err := envInt(getenv, "USAGE_TIMEOUT_MS", 2000)
	if err != nil {
		return nil, err
	}
	maxReq, err := envInt(getenv, "MAX_REQUEST_BODY_BYTES", 2*1024*1024)
	if err != nil {
		return nil, err
	}
	maxScan, err := envInt(getenv, "MAX_USAGE_SCAN_BYTES", 1024*1024)
	if err != nil {
		return nil, err
	}
	maxCarry, err := envInt(getenv, "MAX_SSE_CARRY_BYTES", 64*1024)
	if err != nil {
		return nil, err
	}
	defer_, err := envBool(getenv, "DEFER_REQUEST_HEADERS_ACK", true)
	if err != nil {
		return nil, err
	}
	if checkMS <= 0 || usageMS <= 0 || maxReq <= 0 || maxScan <= 0 || maxCarry <= 0 {
		return nil, fmt.Errorf("timeout and size limits must be positive")
	}

	return &Config{
		PolicyURL:                  policy,
		GRPCPort:                   envStr(getenv, "GRPC_PORT", "50051"),
		HealthPort:                 envStr(getenv, "HEALTH_PORT", "8081"),
		EstimatedTokens:            int64(est),
		UnsupportedGuardrailAction: action,
		CheckTimeout:               time.Duration(checkMS) * time.Millisecond,
		UsageTimeout:               time.Duration(usageMS) * time.Millisecond,
		MaxRequestBodyBytes:        maxReq,
		MaxUsageScanBytes:          maxScan,
		MaxSSECarryBytes:           maxCarry,
		DeferHeadersAck:            defer_,
	}, nil
}
