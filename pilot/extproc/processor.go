package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"log/slog"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	corev3 "github.com/envoyproxy/go-control-plane/envoy/config/core/v3"
	filterv3 "github.com/envoyproxy/go-control-plane/envoy/extensions/filters/http/ext_proc/v3"
	extprocv3 "github.com/envoyproxy/go-control-plane/envoy/service/ext_proc/v3"
	typev3 "github.com/envoyproxy/go-control-plane/envoy/type/v3"
)

// procStream is the half of grpc's ExternalProcessor_ProcessServer this
// processor uses. Narrowed to an interface so the SSE guarantee can be asserted
// by a test that controls exactly when Recv returns -- see
// TestResponseBodyChunkIsForwardedWithoutWaitingForMoreChunks.
type procStream interface {
	Recv() (*extprocv3.ProcessingRequest, error)
	Send(*extprocv3.ProcessingResponse) error
	Context() context.Context
}

// Metrics are process-wide counters. Correctness and latency come first; these
// are plain atomics on the non-realtime paths only.
type Metrics struct {
	StreamsTotal          atomic.Int64
	GuardrailBlocks       atomic.Int64
	GuardrailUnsupported  atomic.Int64
	GuardrailSkips        atomic.Int64
	CheckFailures         atomic.Int64
	UsagePosted           atomic.Int64
	UsageFailures         atomic.Int64
	UsageSkippedZeroDelta atomic.Int64
	UsageMissing          atomic.Int64
	ResponseChunks        atomic.Int64
}

// Processor implements extprocv3.ExternalProcessorServer.
type Processor struct {
	extprocv3.UnimplementedExternalProcessorServer

	cfg     *Config
	client  PolicyClient
	log     *slog.Logger
	metrics *Metrics

	// now is injectable so latency reporting is deterministic in tests.
	now func() time.Time
	// usageWG lets a TEST wait for the fire-and-forget goroutine. It is nil in
	// production and nothing ever waits on it -- that is the entire point.
	usageWG *sync.WaitGroup
}

func NewProcessor(cfg *Config, client PolicyClient, log *slog.Logger, m *Metrics) *Processor {
	return &Processor{cfg: cfg, client: client, log: log, metrics: m, now: time.Now}
}

// exchange is the per-stream state. One gRPC stream == one HTTP exchange, so
// this needs no locking: every field is touched only by the single goroutine
// running the Recv/Send loop, and the ONLY thing that escapes to another
// goroutine is an immutable copy handed to the /v1/usage post.
type exchange struct {
	p *Processor

	apiKey    string
	keyPrefix string // sha256[:12] of the key -- log-safe correlation handle
	path      string
	method    string

	model  string
	tenant string

	// headersAcked tracks the deferred request_headers response.
	headersAcked  bool
	headersDefer  bool
	reqBody       []byte
	reqOverflowed bool

	isSSE  bool
	sse    *SSEUsageScanner
	plain  *BodyAccumulator
	usaged bool

	startedAt   time.Time
	upstreamAt  time.Time
	respChunks  int64
	deniedEarly bool
}

// Process is the bidirectional stream entry point.
//
// EVERYTHING happens on this one goroutine. That is a deliberate design
// constraint, not an accident: gRPC forbids concurrent Send on a stream, and a
// mutex around Send would introduce exactly the "wait on a lock under
// contention" that the SSE requirement forbids. One goroutine, no locks, Send
// first.
func (p *Processor) Process(stream extprocv3.ExternalProcessor_ProcessServer) error {
	return p.process(stream)
}

func (p *Processor) process(stream procStream) error {
	p.metrics.StreamsTotal.Add(1)
	ex := &exchange{p: p, startedAt: p.now()}

	for {
		req, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			ex.finish()
			return nil
		}
		if err != nil {
			ex.finish()
			return err
		}

		// protocol_config rides on the FIRST ProcessingRequest
		// (ext_proc.rs:499 protocol_config_for_headers). It tells us the body
		// modes actually in force, which is what makes the deferred headers ack
		// safe rather than a gamble.
		if pc := req.GetProtocolConfig(); pc != nil {
			ex.headersDefer = p.cfg.DeferHeadersAck &&
				pc.GetRequestBodyMode() == filterv3.ProcessingMode_FULL_DUPLEX_STREAMED
		}

		switch v := req.Request.(type) {
		case *extprocv3.ProcessingRequest_RequestHeaders:
			if err := ex.onRequestHeaders(stream, v.RequestHeaders); err != nil {
				return err
			}
		case *extprocv3.ProcessingRequest_RequestBody:
			if err := ex.onRequestBody(stream, v.RequestBody); err != nil {
				return err
			}
		case *extprocv3.ProcessingRequest_RequestTrailers:
			// The gateway counts trailers as end-of-stream and will NOT send a
			// final empty eos body chunk afterwards (ext_proc.rs:1232-1240,
			// sent_end_stream = true). Finalising here is what keeps a
			// trailered request from hanging forever.
			if err := ex.finalizeRequest(stream); err != nil {
				return err
			}
		case *extprocv3.ProcessingRequest_ResponseHeaders:
			if err := ex.onResponseHeaders(stream, v.ResponseHeaders); err != nil {
				return err
			}
		case *extprocv3.ProcessingRequest_ResponseBody:
			if err := ex.onResponseBody(stream, v.ResponseBody); err != nil {
				return err
			}
		case *extprocv3.ProcessingRequest_ResponseTrailers:
			ex.reportUsage()
			if err := stream.Send(&extprocv3.ProcessingResponse{
				Response: &extprocv3.ProcessingResponse_ResponseTrailers{
					ResponseTrailers: &extprocv3.TrailersResponse{},
				},
			}); err != nil {
				return err
			}
		default:
			// An unknown message type must not stall the exchange.
			p.log.Warn("ignoring unrecognised ProcessingRequest variant")
		}
	}
}

func (ex *exchange) finish() {
	// Terminal safety net: a stream can end without a clean end_of_stream (an
	// aborted download, an upstream reset). Any usage we DID observe is still
	// owed to the tenant's counter.
	ex.reportUsage()
}

// ---------------------------------------------------------------- request

func (ex *exchange) onRequestHeaders(stream procStream, h *extprocv3.HttpHeaders) error {
	hdr := h.GetHeaders()
	ex.apiKey = bearerFrom(headerValue(hdr, "authorization"))
	if ex.apiKey == "" {
		// Some clients send the key as `apikey:`; the plugin server accepts it.
		ex.apiKey = strings.TrimSpace(headerValue(hdr, "apikey"))
	}
	ex.keyPrefix = keyFingerprint(ex.apiKey)
	ex.path = headerValue(hdr, ":path")
	ex.method = headerValue(hdr, ":method")

	// No body: there is nothing to guard and no body message will ever arrive,
	// so deferring the ack here would deadlock the gateway's request loop.
	if h.GetEndOfStream() || !ex.headersDefer {
		return ex.ackRequestHeaders(stream)
	}
	// Deferred. The gateway is already streaming body chunks to us without
	// waiting for this (ext_proc.rs:894-917); see the package comment.
	return nil
}

func (ex *exchange) ackRequestHeaders(stream procStream) error {
	if ex.headersAcked {
		return nil
	}
	ex.headersAcked = true
	return stream.Send(&extprocv3.ProcessingResponse{
		Response: &extprocv3.ProcessingResponse_RequestHeaders{
			RequestHeaders: &extprocv3.HeadersResponse{},
		},
	})
}

func (ex *exchange) onRequestBody(stream procStream, b *extprocv3.HttpBody) error {
	// Buffering the REQUEST body is safe and necessary: guardrails need the
	// whole prompt, and the request body is not the user-visible realtime
	// stream. Kong buffers it too. It is NOT safe to echo request chunks as
	// they arrive: the gateway's request body channel has capacity 1
	// (ext_proc.rs:889) and nothing drains it until mutate_request returns, so
	// echoing more than one chunk early would deadlock the gateway.
	if !ex.reqOverflowed {
		if len(ex.reqBody)+len(b.GetBody()) > ex.p.cfg.MaxRequestBodyBytes {
			ex.reqOverflowed = true
			ex.reqBody = nil
		} else {
			ex.reqBody = append(ex.reqBody, b.GetBody()...)
		}
	}
	if !b.GetEndOfStream() {
		return nil
	}
	return ex.finalizeRequest(stream)
}

func (ex *exchange) finalizeRequest(stream procStream) error {
	if ex.deniedEarly {
		return nil
	}
	if ex.reqOverflowed {
		return ex.deny(stream, Violation{
			StatusCode: 413,
			Code:       "request_too_large",
			Message:    "Request body exceeds the size this gateway will inspect for guardrails.",
		})
	}

	facts := ExtractRequestFacts(ex.reqBody)
	ex.model = facts.Model

	if v := ex.evaluate(facts); v != nil {
		return ex.deny(stream, *v)
	}

	// Allowed. Ack the headers (a no-op if already sent) and then hand back the
	// body EXACTLY as received, in one StreamedResponse with end_of_stream.
	//
	// Order matters: the headers ack is what makes mutate_request return and
	// dispatch upstream; the body response then flows through the continuation
	// task (ext_proc.rs:1130-1163) into the upstream request body channel.
	if err := ex.ackRequestHeaders(stream); err != nil {
		return err
	}
	ex.upstreamAt = ex.p.now()
	return stream.Send(&extprocv3.ProcessingResponse{
		Response: &extprocv3.ProcessingResponse_RequestBody{
			RequestBody: &extprocv3.BodyResponse{
				Response: &extprocv3.CommonResponse{
					BodyMutation: &extprocv3.BodyMutation{
						Mutation: &extprocv3.BodyMutation_StreamedResponse{
							StreamedResponse: &extprocv3.StreamedBodyResponse{
								Body:        ex.reqBody,
								EndOfStream: true,
							},
						},
					},
				},
			},
		},
	})
}

// evaluate fetches this key+model's guardrail directives and enforces them.
// Returns nil to allow.
func (ex *exchange) evaluate(facts RequestFacts) *Violation {
	p := ex.p

	ctx, cancel := context.WithTimeout(context.Background(), p.cfg.CheckTimeout)
	defer cancel()

	resp, err := p.client.Check(ctx, CheckRequest{
		APIKey:          ex.apiKey,
		Model:           facts.Model,
		EstimatedTokens: 0,
		Stages:          []string{"authn"},
	})
	if err != nil {
		// FAIL CLOSED. Consistent with the rest of the pilot: the
		// extauthz-adapter denies with 503 on every error path, and a gateway
		// that forwards prompts unguarded whenever the policy server hiccups is
		// not a guardrail.
		p.metrics.CheckFailures.Add(1)
		p.log.Error("guardrail lookup failed; denying (fail closed)",
			"key_prefix", ex.keyPrefix, "model", facts.Model, "error", err.Error())
		return &Violation{
			StatusCode: 503,
			Code:       "guardrail_unavailable",
			Message:    "The gateway could not verify this request's guardrail policy and will not forward it.",
		}
	}
	if !resp.Allowed {
		// The extauthz-adapter already allowed this request, so reaching here
		// means state changed mid-flight (key revoked, model de-ACLed). Honour
		// the newer verdict.
		status := resp.StatusCode
		if status < 400 || status > 599 {
			status = 403
		}
		p.log.Warn("policy server denied at the guardrail lookup",
			"key_prefix", ex.keyPrefix, "model", facts.Model,
			"stage", resp.Stage, "reason", resp.Reason)
		return &Violation{
			StatusCode: status,
			Code:       "policy_denied",
			Message:    "Request denied by gateway policy.",
		}
	}
	ex.tenant = resp.TenantID

	if len(resp.Guardrails) == 0 {
		return nil
	}
	before := p.metrics.GuardrailSkips.Load()
	v := EvaluateGuardrails(resp.Guardrails, facts.Prompt, p.cfg.UnsupportedGuardrailAction, skipCounter{p})
	_ = before
	if v == nil {
		return nil
	}
	if v.Code == "guardrail_unsupported" {
		p.metrics.GuardrailUnsupported.Add(1)
	}
	p.metrics.GuardrailBlocks.Add(1)
	// Log the matched TERM (operator-configured, safe) and never the prompt.
	p.log.Info("guardrail blocked a request",
		"key_prefix", ex.keyPrefix, "tenant", resp.TenantID, "model", facts.Model,
		"method", ex.method, "path", ex.path,
		"guardrail_kind", v.Kind, "matched_term", v.Term, "code", v.Code)
	return v
}

// skipCounter adapts the process logger to guardrailLogger and counts skips.
type skipCounter struct{ p *Processor }

func (s skipCounter) Warn(msg string, args ...any) {
	s.p.metrics.GuardrailSkips.Add(1)
	s.p.log.Warn(msg, args...)
}

// deny emits an ImmediateResponse. Sent as our FIRST message when the headers
// ack was deferred, which makes the gateway's main request loop return it
// (ext_proc.rs:993) without ever dialing the upstream provider.
func (ex *exchange) deny(stream procStream, v Violation) error {
	ex.deniedEarly = true
	return stream.Send(&extprocv3.ProcessingResponse{
		Response: &extprocv3.ProcessingResponse_ImmediateResponse{
			ImmediateResponse: &extprocv3.ImmediateResponse{
				Status: &typev3.HttpStatus{Code: typev3.StatusCode(v.StatusCode)},
				Body:   []byte(v.Body()),
				Headers: &extprocv3.HeaderMutation{
					SetHeaders: []*corev3.HeaderValueOption{{
						Header: &corev3.HeaderValue{
							Key:      "content-type",
							RawValue: []byte("application/json"),
						},
					}},
				},
				Details: "pilot_extproc_" + v.Code,
			},
		},
	})
}

// ---------------------------------------------------------------- response

func (ex *exchange) onResponseHeaders(stream procStream, h *extprocv3.HttpHeaders) error {
	ct := strings.ToLower(headerValue(h.GetHeaders(), "content-type"))
	ex.isSSE = strings.Contains(ct, "text/event-stream")
	if ex.isSSE {
		ex.sse = NewSSEUsageScanner(ex.p.cfg.MaxSSECarryBytes)
	} else {
		ex.plain = NewBodyAccumulator(ex.p.cfg.MaxUsageScanBytes)
	}
	// CONTINUE immediately, unmodified.
	return stream.Send(&extprocv3.ProcessingResponse{
		Response: &extprocv3.ProcessingResponse_ResponseHeaders{
			ResponseHeaders: &extprocv3.HeadersResponse{},
		},
	})
}

// onResponseBody is THE realtime path. Read the ordering here as a contract.
func (ex *exchange) onResponseBody(stream procStream, b *extprocv3.HttpBody) error {
	body := b.GetBody()
	eos := b.GetEndOfStream()

	// ---- STEP 1, ALWAYS FIRST: hand the chunk straight back, byte for byte.
	//
	// A BodyResponse with no mutation is NOT equivalent: agentgateway reads
	// `ResponseBody(BodyResponse{response: None})` as end-of-stream
	// (ext_proc/mutation.rs:248-251) and would truncate the stream. The chunk
	// must be echoed as a StreamedResponse carrying the ORIGINAL bytes.
	//
	// `body` is not copied and not mutated -- passing the same slice back keeps
	// the gateway off its streamed-body-mutation rewrite path and costs one
	// marshal, which grpc does synchronously inside Send.
	if err := stream.Send(&extprocv3.ProcessingResponse{
		Response: &extprocv3.ProcessingResponse_ResponseBody{
			ResponseBody: &extprocv3.BodyResponse{
				Response: &extprocv3.CommonResponse{
					BodyMutation: &extprocv3.BodyMutation{
						Mutation: &extprocv3.BodyMutation_StreamedResponse{
							StreamedResponse: &extprocv3.StreamedBodyResponse{
								Body:        body,
								EndOfStream: eos,
							},
						},
					},
				},
			},
		},
	}); err != nil {
		return err
	}

	// ---- STEP 2: only now, observe. Bounded, allocation-light, no I/O, no
	// locks, no channels. Nothing below can delay a chunk that has already been
	// forwarded, and nothing below is allowed to grow with stream length beyond
	// its configured cap.
	ex.respChunks++
	ex.p.metrics.ResponseChunks.Add(1)
	if ex.sse != nil {
		ex.sse.Feed(body)
	} else if ex.plain != nil {
		ex.plain.Feed(body)
	}

	// ---- STEP 3: at end of stream, fire and forget. reportUsage never blocks.
	if eos {
		ex.reportUsage()
	}
	return nil
}

// reportUsage computes the true-up and posts it WITHOUT blocking the stream.
// Idempotent: called from end-of-stream, from response trailers, and from the
// stream teardown safety net.
func (ex *exchange) reportUsage() {
	if ex.usaged {
		return
	}
	ex.usaged = true
	p := ex.p

	if ex.apiKey == "" || ex.model == "" {
		return
	}

	var u Usage
	switch {
	case ex.sse != nil:
		u = ex.sse.Result()
	case ex.plain != nil:
		u = ex.plain.Result()
	default:
		return
	}
	if !u.Found {
		// Nothing observed: an error response, a provider that omits usage, or
		// a scan that overflowed. Posting a guess would corrupt the counter, so
		// leave the flat estimate standing and make the gap visible.
		p.metrics.UsageMissing.Add(1)
		p.log.Info("no usage found in response; TPM stays at the flat estimate",
			"key_prefix", ex.keyPrefix, "model", ex.model,
			"streaming", ex.sse != nil, "chunks", ex.respChunks)
		return
	}

	total := u.Total()
	delta := Delta(total, p.cfg.EstimatedTokens)
	if delta == 0 {
		// The server short-circuits a zero delta before any work; skipping the
		// round trip entirely is strictly better.
		p.metrics.UsageSkippedZeroDelta.Add(1)
		return
	}

	prompt, completion, tot := u.PromptTokens, u.CompletionTokens, total
	req := UsageRequest{
		APIKey:           ex.apiKey,
		Model:            ex.model,
		DeltaTokens:      delta,
		PromptTokens:     &prompt,
		CompletionTokens: &completion,
		TotalTokens:      &tot,
	}
	if !ex.upstreamAt.IsZero() {
		req.UpstreamLatency = float64(p.now().Sub(ex.upstreamAt).Microseconds()) / 1000.0
	}
	if !ex.startedAt.IsZero() {
		req.ProxyLatency = float64(p.now().Sub(ex.startedAt).Microseconds()) / 1000.0
	}

	keyPrefix, model, chunks, tenant := ex.keyPrefix, ex.model, ex.respChunks, ex.tenant
	wg := p.usageWG
	if wg != nil {
		wg.Add(1)
	}
	go func() {
		if wg != nil {
			defer wg.Done()
		}
		// Its OWN context: deriving from the stream context would cancel the
		// true-up the instant the client disconnects, which is exactly when a
		// long stream has produced the largest correction.
		ctx, cancel := context.WithTimeout(context.Background(), p.cfg.UsageTimeout)
		defer cancel()
		if err := p.client.Usage(ctx, req); err != nil {
			// A failed true-up must never affect the response -- it has already
			// been fully delivered. Count it and move on.
			p.metrics.UsageFailures.Add(1)
			p.log.Error("TPM true-up failed",
				"key_prefix", keyPrefix, "model", model, "delta_tokens", req.DeltaTokens,
				"error", err.Error())
			return
		}
		p.metrics.UsagePosted.Add(1)
		p.log.Info("TPM true-up posted",
			"key_prefix", keyPrefix, "tenant", tenant, "model", model,
			"delta_tokens", req.DeltaTokens, "total_tokens", tot,
			"prompt_tokens", prompt, "completion_tokens", completion,
			"chunks", chunks)
	}()
}

// ---------------------------------------------------------------- helpers

// headerValue reads a header case-insensitively.
//
// agentgateway puts REAL header values in raw_value (bytes) and PSEUDO header
// values (:path, :method, :status) in value (string) --
// http/ext_proc/headers.rs:66-79. Reading only one of the two silently returns
// empty for half the headers, so both are checked.
func headerValue(hm *corev3.HeaderMap, name string) string {
	if hm == nil {
		return ""
	}
	for _, h := range hm.GetHeaders() {
		if !strings.EqualFold(h.GetKey(), name) {
			continue
		}
		if raw := h.GetRawValue(); len(raw) > 0 {
			return string(raw)
		}
		return h.GetValue()
	}
	return ""
}

func bearerFrom(authz string) string {
	authz = strings.TrimSpace(authz)
	if authz == "" {
		return ""
	}
	if len(authz) >= 7 && strings.EqualFold(authz[:7], "bearer ") {
		return strings.TrimSpace(authz[7:])
	}
	return authz
}

// keyFingerprint is the ONLY representation of the api key that may be logged.
// The raw key, the bearer header and prompt content never appear in a log line.
// 12 hex characters of the sha256 is enough to correlate requests and far too
// little to attack offline.
func keyFingerprint(key string) string {
	if key == "" {
		return ""
	}
	sum := sha256.Sum256([]byte(key))
	return hex.EncodeToString(sum[:])[:12]
}
