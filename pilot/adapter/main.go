// Command extauthz-adapter bridges two incompatible authorization contracts.
//
// The upstream Rust service (ai-gateway-plugin-server) ALWAYS answers HTTP 200
// and carries the real allow/deny decision inside the JSON body. agentgateway's
// HTTP ext_authz, by contrast, derives allow/deny purely from the HTTP status
// code of the auth service. Wiring them together directly would fail OPEN:
// every request, including denials, would be allowed.
//
// This adapter translates body-encoded decisions into status codes.
//
// FAIL-CLOSED IS THE SINGLE MOST IMPORTANT PROPERTY OF THIS SERVICE.
// Every error path below - body read error, transport error, non-200 upstream
// status, malformed JSON - MUST deny with 503. There is exactly one code path
// in this file that emits a 2xx (see allow() ), and it is only reachable after
// the upstream explicitly said {"allowed": true}. Do not add another.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strconv"
	"time"
)

// rateLimitState is one (scope, dimension, window) entry of the decision's
// `rate_limits` array. Mirrors RateLimitStateDto in the plugin server
// (crates/server/src/http/handlers/schemas.rs).
type rateLimitState struct {
	Scope     string `json:"scope"`
	Dimension string `json:"dimension"`
	Window    string `json:"window"`
	Limit     int64  `json:"limit"`
	Remaining int64  `json:"remaining"`
	Reset     uint64 `json:"reset"`
}

// decision is the JSON body returned by the Rust plugin server. It mirrors
// CheckResponse in crates/server/src/http/handlers/schemas.rs.
//
// The field names and types here are LOAD BEARING and were wrong until the
// pilot's first-ever successful authorization exposed it: this struct used to
// declare `tenant` (the real field is `tenant_id`) and
// `rate_limits map[string]string` (the real field is an ARRAY of
// rateLimitState). Because a DENY carries neither field, every test and every
// live request passed for as long as nothing was ever allowed. The first allow
// hit `cannot unmarshal array into Go struct field decision.rate_limits` and
// fail-closed with 503. Verify any change to this struct against the Rust
// source, not against intuition.
type decision struct {
	Allowed    bool             `json:"allowed"`
	StatusCode int              `json:"status_code"`
	Stage      string           `json:"stage"`
	Reason     string           `json:"reason"`
	TenantID   string           `json:"tenant_id"`
	RateLimits []rateLimitState `json:"rate_limits"`
}

// forwardedHeaders are copied from the inbound request to the upstream check
// call when present and non-empty.
var forwardedHeaders = []string{
	"Authorization",
	"X-Model",
	"X-Estimated-Tokens",
	"X-Request-Id",
}

const (
	// statusFailClosed is returned for EVERY error path. 5xx is a deny as far
	// as ext_authz is concerned, which is what we want when we cannot obtain a
	// trustworthy decision.
	statusFailClosed = http.StatusServiceUnavailable
	// statusDefaultDeny is used when the upstream denies without naming a
	// usable status code.
	statusDefaultDeny = http.StatusForbidden
)

// denyClosed rejects the request because we could not obtain a trustworthy
// decision. This is the only error sink in the handler.
func denyClosed(w http.ResponseWriter, reason string, err error) {
	log.Printf("ext_authz fail-closed: %s: %v", reason, err)
	w.WriteHeader(statusFailClosed)
}

// newHandler returns the ext_authz translation handler pointed at serverURL,
// the base URL of the Rust plugin server (its /v1/check endpoint is appended).
func newHandler(serverURL string) http.Handler {
	target := serverURL + "/v1/check"
	client := &http.Client{Timeout: 3 * time.Second}

	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			denyClosed(w, "reading request body", err)
			return
		}

		req, err := http.NewRequestWithContext(r.Context(), http.MethodPost, target, bytes.NewReader(body))
		if err != nil {
			denyClosed(w, "building upstream request", err)
			return
		}
		req.Header.Set("Content-Type", "application/json")
		for _, h := range forwardedHeaders {
			if v := r.Header.Get(h); v != "" {
				req.Header.Set(h, v)
			}
		}

		resp, err := client.Do(req)
		if err != nil {
			denyClosed(w, "upstream transport error", err)
			return
		}
		defer resp.Body.Close()

		// The upstream contract is "always 200". Anything else means the
		// service is broken or misconfigured, so we must not guess - deny.
		if resp.StatusCode != http.StatusOK {
			denyClosed(w, "upstream returned non-200 (contract violation)", fmt.Errorf("status %d", resp.StatusCode))
			return
		}

		var d decision
		if err := json.NewDecoder(resp.Body).Decode(&d); err != nil {
			denyClosed(w, "decoding upstream decision", err)
			return
		}

		if !d.Allowed {
			deny(w, d)
			return
		}
		allow(w, d)
	})
}

// setRateLimitHeaders emits X-RateLimit-* for every window the plugin server
// reported, and returns the Retry-After value (seconds) if any window is
// exhausted.
//
// Header naming is deliberately byte-identical to the Kong plugin's
// build_ratelimit_headers (kong-plugin/kong/plugins/ai-gateway/handler.lua):
//
//	X-RateLimit-{Limit,Remaining,Reset}-{scope}-{dimension}-{window}
//
// Clients migrating off Kong must not have to relearn header names.
//
// Retry-After is the LONGEST reset among EXHAUSTED windows only: the caller can
// retry once every tripped window has cleared, and a healthy long window must
// not inflate the backoff.
func setRateLimitHeaders(w http.ResponseWriter, states []rateLimitState) {
	var retryAfter uint64
	var exhausted bool
	for _, s := range states {
		suffix := fmt.Sprintf("%s-%s-%s", s.Scope, s.Dimension, s.Window)
		w.Header().Set("X-RateLimit-Limit-"+suffix, strconv.FormatInt(s.Limit, 10))
		w.Header().Set("X-RateLimit-Remaining-"+suffix, strconv.FormatInt(s.Remaining, 10))
		w.Header().Set("X-RateLimit-Reset-"+suffix, strconv.FormatUint(s.Reset, 10))
		if s.Remaining <= 0 && s.Reset > retryAfter {
			retryAfter, exhausted = s.Reset, true
		}
	}
	if exhausted {
		w.Header().Set("Retry-After", strconv.FormatUint(retryAfter, 10))
	}
}

// deny translates a body-encoded denial into an HTTP status code.
func deny(w http.ResponseWriter, d decision) {
	if d.Reason != "" {
		w.Header().Set("X-Auth-Reason", d.Reason)
	}
	// Rate-limit state is surfaced on deny too: a 429 is exactly when the
	// client most needs to know which window it hit and when it resets.
	setRateLimitHeaders(w, d.RateLimits)
	// Only honour a status the upstream supplied if it actually denies.
	// A 2xx (or nonsense like 999) in a denial would fail OPEN, so clamp it.
	status := statusDefaultDeny
	if d.StatusCode >= 400 && d.StatusCode <= 599 {
		status = d.StatusCode
	}
	w.WriteHeader(status)
}

// allow is the ONLY path in this program that emits a 2xx.
func allow(w http.ResponseWriter, d decision) {
	if d.TenantID != "" {
		w.Header().Set("X-Tenant-ID", d.TenantID)
	}
	setRateLimitHeaders(w, d.RateLimits)
	w.WriteHeader(http.StatusOK)
}

func main() {
	serverURL := os.Getenv("RUST_SERVER_URL")
	if serverURL == "" {
		log.Fatal("RUST_SERVER_URL is required")
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	mux := http.NewServeMux()
	mux.Handle("/check", newHandler(serverURL))
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})

	addr := ":" + port
	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	log.Printf("ext_authz adapter listening on %s, target %s/v1/check", addr, serverURL)
	log.Fatal(srv.ListenAndServe())
}
