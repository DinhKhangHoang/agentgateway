package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"
)

// CheckRequest is the /v1/check body. Field names and types mirror
// ai-gateway-plugin-server's schemas.rs CheckRequest exactly.
type CheckRequest struct {
	APIKey string `json:"api_key"`
	Model  string `json:"model"`
	// EstimatedTokens is deliberately 0 here. It is ONLY consumed by the
	// `ratelimit` stage, which we do not run (see Stages), so any other value
	// would be dead weight that a future server version might start honouring.
	EstimatedTokens int64 `json:"estimated_tokens"`
	// Stages is ALWAYS exactly ["authn"]. This is the single most important
	// line in this file.
	//
	// The extauthz-adapter (pilot/10-adapter.yaml) has already called /v1/check
	// with ["authn","acl","ratelimit"] for this same request, which PRE-DEBITED
	// estimated_tokens=100 against the tenant's TPM budget. Adding "ratelimit"
	// here would debit a second time and quietly halve every tenant's effective
	// quota -- a correctness bug that no test at the gateway would catch.
	//
	// "guardrails" is also absent, and that is not an oversight: the guardrails
	// stage is an attach-only no-op that enforces nothing, and
	// CheckResponse.guardrails is populated from KeyConfig.guardrails[model]
	// gated ONLY on decision.allow (schemas.rs:148) -- it does not depend on
	// the stage being listed. So ["authn"] returns the directives we need with
	// no rate-limit side effect. It is also cheap: the plugin server caches the
	// KeyConfig in-process for 600s (APP_CACHE_TTL_SECS).
	Stages []string `json:"stages"`
}

// CheckResponse is the /v1/check reply. HTTP is ALWAYS 200; the verdict is here.
type CheckResponse struct {
	Allowed    bool                 `json:"allowed"`
	StatusCode int                  `json:"status_code"`
	Stage      string               `json:"stage"`
	Reason     string               `json:"reason"`
	TenantID   string               `json:"tenant_id"`
	Guardrails []GuardrailDirective `json:"guardrails"`
}

// UsageRequest is the /v1/usage body -- the log-phase TPM true-up.
type UsageRequest struct {
	APIKey string `json:"api_key"`
	// Model MUST byte-match what was sent to /v1/check or the per-model TPM
	// counters will not line up and the correction lands on a phantom counter.
	Model string `json:"model"`
	// DeltaTokens is actual_total - estimated. Required; 0 short-circuits
	// server-side before any work.
	DeltaTokens      int64   `json:"delta_tokens"`
	PromptTokens     *int64  `json:"prompt_tokens,omitempty"`
	CompletionTokens *int64  `json:"completion_tokens,omitempty"`
	TotalTokens      *int64  `json:"total_tokens,omitempty"`
	UpstreamLatency  float64 `json:"upstream_latency_ms,omitempty"`
	ProxyLatency     float64 `json:"proxy_latency_ms,omitempty"`
}

// PolicyClient is the plugin-server surface the processor depends on. An
// interface so the processor's fail-closed and fire-and-forget behaviours are
// testable without a live server.
type PolicyClient interface {
	Check(ctx context.Context, req CheckRequest) (*CheckResponse, error)
	Usage(ctx context.Context, req UsageRequest) error
}

// HTTPPolicyClient talks to ai-gateway-plugin-server.
type HTTPPolicyClient struct {
	BaseURL string
	Client  *http.Client
}

func NewHTTPPolicyClient(baseURL string) *HTTPPolicyClient {
	return &HTTPPolicyClient{
		BaseURL: baseURL,
		Client: &http.Client{
			// No global timeout: each call carries its own context deadline, so
			// /v1/check and /v1/usage can differ (3s vs Kong's 2s).
			Transport: &http.Transport{
				Proxy: http.ProxyFromEnvironment,
				DialContext: (&net.Dialer{
					Timeout:   2 * time.Second,
					KeepAlive: 30 * time.Second,
				}).DialContext,
				// Keep connections warm: /v1/check is on the request critical
				// path and a TCP handshake per request would be visible in TTFT.
				MaxIdleConns:        100,
				MaxIdleConnsPerHost: 100,
				IdleConnTimeout:     90 * time.Second,
			},
		},
	}
}

func (c *HTTPPolicyClient) Check(ctx context.Context, req CheckRequest) (*CheckResponse, error) {
	body, err := json.Marshal(req)
	if err != nil {
		return nil, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, c.BaseURL+"/v1/check", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	// The api key travels in the BODY only. Sending it as a bearer would put it
	// in a header that any intermediate proxy or access log might capture.
	resp, err := c.Client.Do(httpReq)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		// The contract is "always 200". Anything else is a broken server and
		// must NOT be read as an allow.
		return nil, fmt.Errorf("/v1/check returned HTTP %d", resp.StatusCode)
	}
	var out CheckResponse
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("/v1/check response is not valid JSON: %w", err)
	}
	return &out, nil
}

func (c *HTTPPolicyClient) Usage(ctx context.Context, req UsageRequest) error {
	body, err := json.Marshal(req)
	if err != nil {
		return err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, c.BaseURL+"/v1/usage", bytes.NewReader(body))
	if err != nil {
		return err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	resp, err := c.Client.Do(httpReq)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("/v1/usage returned HTTP %d", resp.StatusCode)
	}
	var out struct {
		OK bool `json:"ok"`
	}
	if err := json.Unmarshal(raw, &out); err != nil {
		return fmt.Errorf("/v1/usage response is not valid JSON: %w", err)
	}
	if !out.OK {
		return fmt.Errorf("/v1/usage reported ok=false")
	}
	return nil
}
