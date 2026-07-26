package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"strings"
	"sync"
	"testing"
	"time"

	corev3 "github.com/envoyproxy/go-control-plane/envoy/config/core/v3"
	filterv3 "github.com/envoyproxy/go-control-plane/envoy/extensions/filters/http/ext_proc/v3"
	extprocv3 "github.com/envoyproxy/go-control-plane/envoy/service/ext_proc/v3"
)

// ---------------------------------------------------------------- harness

// fakeStream drives the processor. Recv pulls from a channel, so a test can
// hold the NEXT chunk back indefinitely and observe whether the processor
// already responded to the previous one. That is the whole mechanism behind the
// SSE regression test.
type fakeStream struct {
	in  chan *extprocv3.ProcessingRequest
	out chan *extprocv3.ProcessingResponse
	ctx context.Context

	mu       sync.Mutex
	sendErr  error
	sentAll  []*extprocv3.ProcessingResponse
	closedIn bool
}

func newFakeStream() *fakeStream {
	return &fakeStream{
		in:  make(chan *extprocv3.ProcessingRequest, 64),
		out: make(chan *extprocv3.ProcessingResponse, 256),
		ctx: context.Background(),
	}
}

func (f *fakeStream) Recv() (*extprocv3.ProcessingRequest, error) {
	req, ok := <-f.in
	if !ok {
		return nil, io.EOF
	}
	return req, nil
}

func (f *fakeStream) Send(r *extprocv3.ProcessingResponse) error {
	f.mu.Lock()
	err := f.sendErr
	if err == nil {
		f.sentAll = append(f.sentAll, r)
	}
	f.mu.Unlock()
	if err != nil {
		return err
	}
	f.out <- r
	return nil
}

func (f *fakeStream) Context() context.Context { return f.ctx }

func (f *fakeStream) closeIn() {
	if !f.closedIn {
		f.closedIn = true
		close(f.in)
	}
}

// next waits for one response, failing the test on timeout. The timeout IS the
// assertion in the realtime tests.
func (f *fakeStream) next(t *testing.T, within time.Duration) *extprocv3.ProcessingResponse {
	t.Helper()
	select {
	case r := <-f.out:
		return r
	case <-time.After(within):
		t.Fatalf("no ProcessingResponse within %s", within)
		return nil
	}
}

type fakeClient struct {
	mu sync.Mutex

	checkResp *CheckResponse
	checkErr  error
	checkReqs []CheckRequest

	usageErr  error
	usageReqs []UsageRequest
	usageHook func()
}

func (c *fakeClient) Check(ctx context.Context, req CheckRequest) (*CheckResponse, error) {
	c.mu.Lock()
	c.checkReqs = append(c.checkReqs, req)
	resp, err := c.checkResp, c.checkErr
	c.mu.Unlock()
	if err != nil {
		return nil, err
	}
	if resp == nil {
		return &CheckResponse{Allowed: true, StatusCode: 200, TenantID: "11377"}, nil
	}
	return resp, nil
}

func (c *fakeClient) Usage(ctx context.Context, req UsageRequest) error {
	c.mu.Lock()
	c.usageReqs = append(c.usageReqs, req)
	err := c.usageErr
	hook := c.usageHook
	c.mu.Unlock()
	if hook != nil {
		hook()
	}
	return err
}

func (c *fakeClient) usageCalls() []UsageRequest {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]UsageRequest(nil), c.usageReqs...)
}

func (c *fakeClient) checkCalls() []CheckRequest {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]CheckRequest(nil), c.checkReqs...)
}

func testConfig() *Config {
	cfg, err := LoadConfig(func(k string) string {
		if k == "POLICY_SERVER_URL" {
			return "http://policy:8080"
		}
		return ""
	})
	if err != nil {
		panic(err)
	}
	return cfg
}

// newTestProcessor returns a processor plus a wait function for the
// fire-and-forget /v1/usage goroutine.
func newTestProcessor(t *testing.T, cfg *Config, c PolicyClient) (*Processor, func()) {
	t.Helper()
	if cfg == nil {
		cfg = testConfig()
	}
	var wg sync.WaitGroup
	p := NewProcessor(cfg, c, slog.New(slog.NewTextHandler(io.Discard, nil)), &Metrics{})
	// The processor Adds before spawning, so waiting here is deterministic.
	p.usageWG = &wg
	return p, wg.Wait
}

func headersMsg(kv map[string]string, eos bool, withProtocolConfig bool) *extprocv3.ProcessingRequest {
	var hs []*corev3.HeaderValue
	for k, v := range kv {
		if strings.HasPrefix(k, ":") {
			// agentgateway sends pseudo-headers in `value`.
			hs = append(hs, &corev3.HeaderValue{Key: k, Value: v})
			continue
		}
		// ...and real headers in `raw_value`.
		hs = append(hs, &corev3.HeaderValue{Key: k, RawValue: []byte(v)})
	}
	req := &extprocv3.ProcessingRequest{
		Request: &extprocv3.ProcessingRequest_RequestHeaders{
			RequestHeaders: &extprocv3.HttpHeaders{
				Headers:     &corev3.HeaderMap{Headers: hs},
				EndOfStream: eos,
			},
		},
	}
	if withProtocolConfig {
		req.ProtocolConfig = &extprocv3.ProtocolConfiguration{
			RequestBodyMode:  filterv3.ProcessingMode_FULL_DUPLEX_STREAMED,
			ResponseBodyMode: filterv3.ProcessingMode_FULL_DUPLEX_STREAMED,
		}
	}
	return req
}

func reqBodyMsg(b string, eos bool) *extprocv3.ProcessingRequest {
	return &extprocv3.ProcessingRequest{
		Request: &extprocv3.ProcessingRequest_RequestBody{
			RequestBody: &extprocv3.HttpBody{Body: []byte(b), EndOfStream: eos},
		},
	}
}

func respHeadersMsg(contentType string) *extprocv3.ProcessingRequest {
	return &extprocv3.ProcessingRequest{
		Request: &extprocv3.ProcessingRequest_ResponseHeaders{
			ResponseHeaders: &extprocv3.HttpHeaders{
				Headers: &corev3.HeaderMap{Headers: []*corev3.HeaderValue{
					{Key: ":status", Value: "200"},
					{Key: "content-type", RawValue: []byte(contentType)},
				}},
			},
		},
	}
}

func respBodyMsg(b string, eos bool) *extprocv3.ProcessingRequest {
	return &extprocv3.ProcessingRequest{
		Request: &extprocv3.ProcessingRequest_ResponseBody{
			ResponseBody: &extprocv3.HttpBody{Body: []byte(b), EndOfStream: eos},
		},
	}
}

const chatBody = `{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"hello there"}],"stream":true}`

// ============================================================ SSE GUARANTEE

// THE regression test. It asserts the property the whole design exists to
// protect: the processor answers a response body chunk WITHOUT having seen the
// next one.
//
// The fake stream deliberately supplies exactly ONE body chunk and then blocks
// forever in Recv. If anyone ever adds buffering -- accumulate-and-flush,
// coalescing, a look-ahead, a wait on a full channel -- the echo for chunk 1
// will not appear and this test fails on the timeout.
func TestResponseBodyChunkIsForwardedWithoutWaitingForMoreChunks(t *testing.T) {
	fs := newFakeStream()
	client := &fakeClient{}
	p, _ := newTestProcessor(t, nil, client)

	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{
		":path": "/v1/chat/completions", ":method": "POST", "authorization": "Bearer sk-test",
	}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)

	// Drain the request-phase responses (headers ack + body).
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)

	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)

	const chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"
	sentAt := time.Now()
	fs.in <- respBodyMsg(chunk, false)
	// NOTE: no further messages are queued. Recv now blocks indefinitely.

	// A buffering processor cannot pass this: it has nothing else to wait for.
	resp := fs.next(t, 500*time.Millisecond)
	elapsed := time.Since(sentAt)

	body := resp.GetResponseBody()
	if body == nil {
		t.Fatalf("expected a ResponseBody reply, got %T", resp.Response)
	}
	sr := body.GetResponse().GetBodyMutation().GetStreamedResponse()
	if sr == nil {
		t.Fatal("a response body chunk MUST be echoed as a StreamedResponse; a nil body mutation is read by agentgateway as end-of-stream and truncates the client's stream")
	}
	if string(sr.GetBody()) != chunk {
		t.Fatalf("chunk was modified.\n got: %q\nwant: %q", sr.GetBody(), chunk)
	}
	if sr.GetEndOfStream() {
		t.Fatal("end_of_stream must mirror the inbound chunk, which was false")
	}
	if elapsed > 250*time.Millisecond {
		t.Fatalf("chunk took %s to be forwarded; something on the realtime path is blocking", elapsed)
	}

	fs.closeIn()
	<-done
}

// Every chunk must come back byte-identical and in order.
func TestResponseBodyChunksAreForwardedUnmodifiedAndInOrder(t *testing.T) {
	fs := newFakeStream()
	client := &fakeClient{}
	p, waitUsage := newTestProcessor(t, nil, client)

	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{":path": "/v1/chat/completions", "authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)

	chunks := []string{
		"data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
		"data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
		"data: {\"choices\":[{\"delta\":{\"content\":\"three\"}}]}\n\n",
		"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":20,\"total_tokens\":25}}\n\n",
		"data: [DONE]\n\n",
	}
	var got []string
	for i, c := range chunks {
		eos := i == len(chunks)-1
		fs.in <- respBodyMsg(c, eos)
		resp := fs.next(t, 2*time.Second)
		sr := resp.GetResponseBody().GetResponse().GetBodyMutation().GetStreamedResponse()
		if sr == nil {
			t.Fatalf("chunk %d: missing StreamedResponse", i)
		}
		if sr.GetEndOfStream() != eos {
			t.Fatalf("chunk %d: end_of_stream = %v, want %v", i, sr.GetEndOfStream(), eos)
		}
		got = append(got, string(sr.GetBody()))
	}
	for i := range chunks {
		if got[i] != chunks[i] {
			t.Fatalf("chunk %d altered.\n got: %q\nwant: %q", i, got[i], chunks[i])
		}
	}

	fs.closeIn()
	<-done
	waitUsage()
}

// ============================================================ GUARDRAILS

func TestKeywordGuardrailHitProducesImmediate403(t *testing.T) {
	kw, _ := json.Marshal(map[string]any{"keywords": []string{"vng-secret-project"}})
	client := &fakeClient{checkResp: &CheckResponse{
		Allowed: true, StatusCode: 200, TenantID: "11377",
		Guardrails: []GuardrailDirective{{Kind: "keyword", Plugin: "keyword-guard-request", Config: kw}},
	}}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{":path": "/v1/chat/completions", "authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(`{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"tell me about the vng-secret-project"}]}`, true)

	resp := fs.next(t, 2*time.Second)
	ir := resp.GetImmediateResponse()
	if ir == nil {
		t.Fatalf("expected an ImmediateResponse, got %T", resp.Response)
	}
	if ir.GetStatus().GetCode() != 403 {
		t.Fatalf("status = %d, want 403", ir.GetStatus().GetCode())
	}
	var doc map[string]any
	if err := json.Unmarshal(ir.GetBody(), &doc); err != nil {
		t.Fatalf("403 body is not JSON: %v (%s)", err, ir.GetBody())
	}

	// The deny must be our FIRST message: that is what makes agentgateway's main
	// request loop return it without ever dialing the upstream provider.
	fs.mu.Lock()
	first := fs.sentAll[0]
	n := len(fs.sentAll)
	fs.mu.Unlock()
	if first.GetImmediateResponse() == nil {
		t.Fatal("the ImmediateResponse must be the first message so the provider is never contacted")
	}
	if n != 1 {
		t.Fatalf("sent %d messages before denying, want exactly 1", n)
	}

	// The check must have run authn ONLY.
	calls := client.checkCalls()
	if len(calls) != 1 {
		t.Fatalf("expected 1 /v1/check call, got %d", len(calls))
	}
	if len(calls[0].Stages) != 1 || calls[0].Stages[0] != "authn" {
		t.Fatalf("stages = %v; adding ratelimit here would DOUBLE-DEBIT the tenant's TPM", calls[0].Stages)
	}

	fs.closeIn()
	<-done
}

func TestKeywordGuardrailMissContinues(t *testing.T) {
	kw, _ := json.Marshal(map[string]any{"keywords": []string{"vng-secret-project"}})
	client := &fakeClient{checkResp: &CheckResponse{
		Allowed: true, StatusCode: 200, TenantID: "11377",
		Guardrails: []GuardrailDirective{{Kind: "keyword", Plugin: "keyword-guard-request", Config: kw}},
	}}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	body := `{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"what is the capital of France?"}]}`
	fs.in <- headersMsg(map[string]string{":path": "/v1/chat/completions", "authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(body, true)

	first := fs.next(t, 2*time.Second)
	if first.GetRequestHeaders() == nil {
		t.Fatalf("expected the deferred headers ack first, got %T", first.Response)
	}
	second := fs.next(t, 2*time.Second)
	sr := second.GetRequestBody().GetResponse().GetBodyMutation().GetStreamedResponse()
	if sr == nil {
		t.Fatalf("expected the request body echoed back, got %T", second.Response)
	}
	if string(sr.GetBody()) != body {
		t.Fatalf("request body was altered:\n got: %s\nwant: %s", sr.GetBody(), body)
	}
	if !sr.GetEndOfStream() {
		t.Fatal("the request body reply must carry end_of_stream or the gateway never dispatches upstream")
	}

	fs.closeIn()
	<-done
}

// A request body split across many chunks must be reassembled before matching,
// or a keyword straddling a chunk boundary would slip through.
func TestGuardrailSeesAPromptSplitAcrossRequestChunks(t *testing.T) {
	kw, _ := json.Marshal(map[string]any{"keywords": []string{"vng-secret-project"}})
	client := &fakeClient{checkResp: &CheckResponse{
		Allowed: true, StatusCode: 200,
		Guardrails: []GuardrailDirective{{Kind: "keyword", Plugin: "keyword-guard-request", Config: kw}},
	}}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	body := `{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"about vng-secret-project please"}]}`
	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	for i := 0; i < len(body); i += 7 {
		end := i + 7
		if end > len(body) {
			end = len(body)
		}
		fs.in <- reqBodyMsg(body[i:end], end == len(body))
	}

	resp := fs.next(t, 2*time.Second)
	if resp.GetImmediateResponse() == nil {
		t.Fatalf("a keyword split across request chunks must still block, got %T", resp.Response)
	}
	fs.closeIn()
	<-done
}

func TestCheckUnreachableFailsClosed(t *testing.T) {
	client := &fakeClient{checkErr: errors.New("connection refused")}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)

	resp := fs.next(t, 2*time.Second)
	ir := resp.GetImmediateResponse()
	if ir == nil {
		t.Fatalf("an unreachable policy server MUST deny, got %T", resp.Response)
	}
	if code := ir.GetStatus().GetCode(); code != 503 {
		t.Fatalf("status = %d, want 503", code)
	}
	if !strings.Contains(string(ir.GetBody()), "guardrail_unavailable") {
		t.Fatalf("body = %s", ir.GetBody())
	}
	fs.closeIn()
	<-done
}

func TestPolicyServerDenyIsHonoured(t *testing.T) {
	client := &fakeClient{checkResp: &CheckResponse{Allowed: false, StatusCode: 401, Stage: "authn", Reason: "unknown api key"}}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-bogus"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)

	resp := fs.next(t, 2*time.Second)
	if resp.GetImmediateResponse().GetStatus().GetCode() != 401 {
		t.Fatalf("got %+v", resp.Response)
	}
	fs.closeIn()
	<-done
}

func TestUnsupportedDirectiveDeniesByDefaultThroughTheProcessor(t *testing.T) {
	client := &fakeClient{checkResp: &CheckResponse{
		Allowed: true, StatusCode: 200,
		Guardrails: []GuardrailDirective{{Kind: "llama", Plugin: "llama-ai-guard-request", Config: json.RawMessage(`{}`)}},
	}}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)

	resp := fs.next(t, 2*time.Second)
	ir := resp.GetImmediateResponse()
	if ir == nil || ir.GetStatus().GetCode() != 403 {
		t.Fatalf("unsupported directive must deny by default, got %T", resp.Response)
	}
	if !strings.Contains(string(ir.GetBody()), "guardrail_unsupported") {
		t.Fatalf("body = %s", ir.GetBody())
	}
	fs.closeIn()
	<-done
}

// A request with no body (headers end_of_stream) must be acked IMMEDIATELY --
// deferring would deadlock the gateway, which sends no body message at all.
func TestBodylessRequestIsAckedImmediately(t *testing.T) {
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, &fakeClient{})
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{":path": "/v1/models", ":method": "GET"}, true, true)
	resp := fs.next(t, 500*time.Millisecond)
	if resp.GetRequestHeaders() == nil {
		t.Fatalf("a bodyless request must be acked at once or the gateway hangs, got %T", resp.Response)
	}
	fs.closeIn()
	<-done
}

// If the gateway is not in FULL_DUPLEX_STREAMED we must not defer either.
func TestNonFullDuplexRequestIsAckedImmediately(t *testing.T) {
	fs := newFakeStream()
	p, _ := newTestProcessor(t, nil, &fakeClient{})
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	msg := headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	msg.ProtocolConfig.RequestBodyMode = filterv3.ProcessingMode_BUFFERED
	fs.in <- msg

	resp := fs.next(t, 500*time.Millisecond)
	if resp.GetRequestHeaders() == nil {
		t.Fatalf("deferral is only safe under FULL_DUPLEX_STREAMED, got %T", resp.Response)
	}
	fs.closeIn()
	<-done
}

// ============================================================ TRUE-UP

func TestUsageTrueUpFromStreamingResponse(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n", false)
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(deepseekTerminal, false)
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg("data: [DONE]\n\n", true)
	fs.next(t, 2*time.Second)

	fs.closeIn()
	<-done
	waitUsage()

	calls := client.usageCalls()
	if len(calls) != 1 {
		t.Fatalf("expected exactly 1 /v1/usage post, got %d", len(calls))
	}
	u := calls[0]
	if u.DeltaTokens != 42 {
		t.Fatalf("delta_tokens = %d, want 42 (142 actual - 100 estimated)", u.DeltaTokens)
	}
	if u.Model != "deepseek-v4-pro" {
		t.Fatalf("model = %q; it MUST byte-match what went to /v1/check or the TPM counters do not line up", u.Model)
	}
	if u.TotalTokens == nil || *u.TotalTokens != 142 {
		t.Fatalf("total_tokens = %v", u.TotalTokens)
	}
	if u.PromptTokens == nil || *u.PromptTokens != 18 {
		t.Fatalf("prompt_tokens = %v", u.PromptTokens)
	}
	if u.CompletionTokens == nil || *u.CompletionTokens != 124 {
		t.Fatalf("completion_tokens = %v", u.CompletionTokens)
	}
}

func TestUsageTrueUpFromNonStreamingResponse(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(`{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("application/json")
	fs.next(t, 2*time.Second)

	body := `{"id":"x","choices":[{"message":{"content":"hello"}}],"usage":{"prompt_tokens":9,"completion_tokens":51,"total_tokens":60}}`
	fs.in <- respBodyMsg(body[:50], false)
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(body[50:], true)
	fs.next(t, 2*time.Second)

	fs.closeIn()
	<-done
	waitUsage()

	calls := client.usageCalls()
	if len(calls) != 1 {
		t.Fatalf("expected 1 usage post, got %d", len(calls))
	}
	if calls[0].DeltaTokens != -40 {
		t.Fatalf("delta_tokens = %d, want -40 (60 actual - 100 estimated, a refund)", calls[0].DeltaTokens)
	}
	if calls[0].Model != "gemini-2.5-flash" {
		t.Fatalf("model = %q", calls[0].Model)
	}
}

func TestZeroDeltaShortCircuitsWithoutPosting(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("application/json")
	fs.next(t, 2*time.Second)
	// total_tokens exactly equals the estimate.
	fs.in <- respBodyMsg(`{"usage":{"prompt_tokens":40,"completion_tokens":60,"total_tokens":100}}`, true)
	fs.next(t, 2*time.Second)

	fs.closeIn()
	<-done
	waitUsage()

	if n := len(client.usageCalls()); n != 0 {
		t.Fatalf("delta 0 must not be posted at all, got %d posts", n)
	}
	if p.metrics.UsageSkippedZeroDelta.Load() != 1 {
		t.Fatal("the zero-delta short circuit should be counted")
	}
}

func TestMissingUsageDoesNotPostAGuess(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("application/json")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(`{"error":{"message":"upstream 401"}}`, true)
	fs.next(t, 2*time.Second)

	fs.closeIn()
	<-done
	waitUsage()

	if n := len(client.usageCalls()); n != 0 {
		t.Fatalf("with no usage observed we must post nothing, got %d", n)
	}
	if p.metrics.UsageMissing.Load() != 1 {
		t.Fatal("a missing usage reading must be counted so the gap is visible")
	}
}

// A /v1/usage failure must be invisible to the client: by the time it runs the
// response has already been delivered in full.
func TestUsageFailureNeverAffectsTheResponse(t *testing.T) {
	client := &fakeClient{usageErr: errors.New("policy server exploded")}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(deepseekTerminal, true)

	resp := fs.next(t, 2*time.Second)
	sr := resp.GetResponseBody().GetResponse().GetBodyMutation().GetStreamedResponse()
	if sr == nil || string(sr.GetBody()) != deepseekTerminal || !sr.GetEndOfStream() {
		t.Fatalf("the final chunk must be delivered intact regardless of the true-up: %+v", resp.Response)
	}

	fs.closeIn()
	err := <-done
	if err != nil {
		t.Fatalf("a failed true-up must not fail the stream: %v", err)
	}
	waitUsage()
	if p.metrics.UsageFailures.Load() != 1 {
		t.Fatal("the failure should be counted")
	}
}

// The true-up must not block stream completion even if /v1/usage hangs for
// longer than the whole test would tolerate inline.
func TestUsagePostDoesNotBlockStreamCompletion(t *testing.T) {
	release := make(chan struct{})
	client := &fakeClient{usageHook: func() { <-release }}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)

	fs.in <- respBodyMsg(deepseekTerminal, true)
	// The eos chunk's echo must arrive while /v1/usage is still stuck.
	fs.next(t, 500*time.Millisecond)

	fs.closeIn()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("the stream did not complete while /v1/usage was blocked")
	}
	close(release)
	waitUsage()
}

// The true-up must survive an abrupt stream teardown with no end_of_stream.
func TestUsageIsReportedOnAbruptStreamEnd(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(deepseekTerminal, false) // note: NOT end_of_stream
	fs.next(t, 2*time.Second)

	fs.closeIn() // client hung up
	<-done
	waitUsage()

	if n := len(client.usageCalls()); n != 1 {
		t.Fatalf("usage observed before an abrupt end is still owed to the tenant; got %d posts", n)
	}
}

func TestUsageIsPostedExactlyOnce(t *testing.T) {
	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, nil, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(chatBody, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("text/event-stream")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(deepseekTerminal, true)
	fs.next(t, 2*time.Second)
	// Trailers after eos, then teardown: three separate paths that each call
	// reportUsage.
	fs.in <- &extprocv3.ProcessingRequest{
		Request: &extprocv3.ProcessingRequest_ResponseTrailers{
			ResponseTrailers: &extprocv3.HttpTrailers{},
		},
	}
	fs.next(t, 2*time.Second)
	fs.closeIn()
	<-done
	waitUsage()

	if n := len(client.usageCalls()); n != 1 {
		t.Fatalf("double-posting a true-up double-corrects the counter; got %d posts", n)
	}
}

// ============================================================ HYGIENE

func TestKeyFingerprintNeverExposesTheKey(t *testing.T) {
	key := "sk-super-secret-value"
	fp := keyFingerprint(key)
	if len(fp) != 12 {
		t.Fatalf("fingerprint length = %d, want 12", len(fp))
	}
	if strings.Contains(fp, "secret") || strings.Contains(key, fp) {
		t.Fatal("the fingerprint must not reveal the key")
	}
	if keyFingerprint("") != "" {
		t.Fatal("no key means no fingerprint")
	}
}

func TestHeaderValueReadsRawValueAndPseudoValue(t *testing.T) {
	hm := &corev3.HeaderMap{Headers: []*corev3.HeaderValue{
		{Key: "Authorization", RawValue: []byte("Bearer abc")},
		{Key: ":path", Value: "/v1/chat/completions"},
	}}
	if got := headerValue(hm, "authorization"); got != "Bearer abc" {
		t.Fatalf("raw_value header = %q", got)
	}
	if got := headerValue(hm, ":path"); got != "/v1/chat/completions" {
		t.Fatalf("pseudo header = %q", got)
	}
	if got := headerValue(hm, "missing"); got != "" {
		t.Fatalf("missing header = %q", got)
	}
}

func TestBearerFrom(t *testing.T) {
	for in, want := range map[string]string{
		"Bearer abc":    "abc",
		"bearer abc":    "abc",
		"BEARER abc":    "abc",
		"abc":           "abc",
		"":              "",
		"  Bearer  x  ": "x",
	} {
		if got := bearerFrom(in); got != want {
			t.Fatalf("bearerFrom(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestOversizedRequestBodyIsRejectedNotSilentlyUnguarded(t *testing.T) {
	cfg := testConfig()
	cfg.MaxRequestBodyBytes = 64
	client := &fakeClient{}
	fs := newFakeStream()
	p, _ := newTestProcessor(t, cfg, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(strings.Repeat("A", 200), true)

	resp := fs.next(t, 2*time.Second)
	ir := resp.GetImmediateResponse()
	if ir == nil || ir.GetStatus().GetCode() != 413 {
		t.Fatalf("an uninspectable body must be rejected, not forwarded unguarded: %+v", resp.Response)
	}
	if n := len(client.checkCalls()); n != 0 {
		t.Fatalf("no point calling the policy server for a body we cannot inspect; got %d calls", n)
	}
	fs.closeIn()
	<-done
}

// TestUsageReportingDisabledSuppressesThePost pins the switch that lets
// agentgateway's NATIVE usageReport policy own the TPM true-up.
//
// This is the double-correction guard. With both this service and the native
// policy reporting, the same request debits the tenant's TPM counter twice —
// silently, and in a way no gateway-level test would catch. The body below is
// the exact one TestUsageTrueUpFromNonStreamingResponse posts a -40 delta for,
// so a regression that ignores the flag shows up here as "1 usage post" rather
// than as a subtly wrong number somewhere else.
func TestUsageReportingDisabledSuppressesThePost(t *testing.T) {
	cfg := testConfig()
	cfg.UsageReportingEnabled = false

	client := &fakeClient{}
	fs := newFakeStream()
	p, waitUsage := newTestProcessor(t, cfg, client)
	done := make(chan error, 1)
	go func() { done <- p.process(fs) }()

	fs.in <- headersMsg(map[string]string{"authorization": "Bearer sk-test"}, false, true)
	fs.in <- reqBodyMsg(`{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, true)
	fs.next(t, 2*time.Second)
	fs.next(t, 2*time.Second)
	fs.in <- respHeadersMsg("application/json")
	fs.next(t, 2*time.Second)
	fs.in <- respBodyMsg(`{"usage":{"prompt_tokens":9,"completion_tokens":51,"total_tokens":60}}`, true)
	fs.next(t, 2*time.Second)

	fs.closeIn()
	<-done
	waitUsage()

	if n := len(client.usageCalls()); n != 0 {
		t.Fatalf("usage reporting is disabled; expected 0 posts, got %d", n)
	}
	// The response still flowed: disabling the POST must not disable the echo.
	if p.metrics.ResponseChunks.Load() != 1 {
		t.Fatalf("response chunks = %d, want 1 — the response path must be untouched",
			p.metrics.ResponseChunks.Load())
	}
}

// TestUsageReportingDefaultsToEnabled guards the default. A config typo that
// silently disabled reporting would stop the true-up with no error anywhere.
func TestUsageReportingDefaultsToEnabled(t *testing.T) {
	cfg, err := LoadConfig(func(k string) string {
		if k == "POLICY_SERVER_URL" {
			return "http://plugin:8080"
		}
		return ""
	})
	if err != nil {
		t.Fatalf("LoadConfig: %v", err)
	}
	if !cfg.UsageReportingEnabled {
		t.Fatal("USAGE_REPORTING_ENABLED must default to true")
	}
}
