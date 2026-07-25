package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func upstream(t *testing.T, payload map[string]any, status int) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(status)
		json.NewEncoder(w).Encode(payload)
	}))
}

func TestAllowReturns200WithTenantHeader(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": true, "tenant_id": "acme"}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 200 {
		t.Fatalf("got status %d, want 200", rec.Code)
	}
	if got := rec.Header().Get("X-Tenant-ID"); got != "acme" {
		t.Fatalf("got tenant %q, want %q", got, "acme")
	}
}

func TestDenyMapsBodyStatusCodeToHTTPStatus(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": false, "status_code": 429, "reason": "quota"}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 429 {
		t.Fatalf("got status %d, want 429", rec.Code)
	}
}

func TestDenyWithoutStatusCodeDefaultsTo403(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": false}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 403 {
		t.Fatalf("got status %d, want 403", rec.Code)
	}
}

func TestUpstreamNon200FailsClosedWith503(t *testing.T) {
	srv := upstream(t, map[string]any{}, 500)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}

func TestUnreachableUpstreamFailsClosedWith503(t *testing.T) {
	rec := httptest.NewRecorder()
	newHandler("http://127.0.0.1:1").ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}

// ---------------------------------------------------------------------------
// Additional coverage.
// ---------------------------------------------------------------------------

// rawUpstream serves a fixed status and a verbatim (possibly non-JSON) body.
func rawUpstream(t *testing.T, body string, status int) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(status)
		io.WriteString(w, body)
	}))
}

func serve(t *testing.T, srv *httptest.Server) *httptest.ResponseRecorder {
	t.Helper()
	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))
	return rec
}

// A denial carrying a 2xx status_code is the worst-case bug: honouring it
// verbatim would make ext_authz ALLOW a request the policy server denied.
// It must clamp to 403.
func TestDenyWithSuccessStatusCodeClampsTo403(t *testing.T) {
	for _, code := range []int{200, 204, 302, 399} {
		srv := upstream(t, map[string]any{"allowed": false, "status_code": code}, 200)
		rec := serve(t, srv)
		srv.Close()

		if rec.Code != 403 {
			t.Fatalf("deny with status_code %d: got status %d, want 403", code, rec.Code)
		}
	}
}

// Out-of-range codes (>599, negative) would panic or confuse the proxy.
func TestDenyWithOutOfRangeStatusCodeClampsTo403(t *testing.T) {
	for _, code := range []int{600, 999, -1} {
		srv := upstream(t, map[string]any{"allowed": false, "status_code": code}, 200)
		rec := serve(t, srv)
		srv.Close()

		if rec.Code != 403 {
			t.Fatalf("deny with status_code %d: got status %d, want 403", code, rec.Code)
		}
	}
}

func TestMalformedUpstreamBodyFailsClosedWith503(t *testing.T) {
	srv := rawUpstream(t, "this is not json", 200)
	defer srv.Close()

	if rec := serve(t, srv); rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}

// An empty 200 body is a decode error (io.EOF); it must not be read as "allow".
func TestEmptyUpstreamBodyFailsClosedWith503(t *testing.T) {
	srv := rawUpstream(t, "", 200)
	defer srv.Close()

	if rec := serve(t, srv); rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}

// Valid JSON that simply omits "allowed" must deny, not default to allow.
func TestMissingAllowedFieldDeniesWith403(t *testing.T) {
	srv := rawUpstream(t, `{"tenant_id":"acme"}`, 200)
	defer srv.Close()

	if rec := serve(t, srv); rec.Code != 403 {
		t.Fatalf("got status %d, want 403", rec.Code)
	}
}

func TestDenySetsAuthReasonHeader(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": false, "status_code": 429, "reason": "quota"}, 200)
	defer srv.Close()

	rec := serve(t, srv)
	if got := rec.Header().Get("X-Auth-Reason"); got != "quota" {
		t.Fatalf("got reason %q, want %q", got, "quota")
	}
}

// REGRESSION (found the first time a request was ever ALLOWED end to end).
//
// `rate_limits` is an ARRAY of per-window state objects, not a string map, and
// the tenant field is `tenant_id`, not `tenant`. The struct here previously
// declared `map[string]string` / `tenant`, so the very first allow failed to
// decode and the adapter fail-closed with 503 — a bug that was structurally
// unreachable while every request denied (a deny carries neither field).
//
// The body below is the real CheckResponse shape, verified against
// crates/server/src/http/handlers/schemas.rs (CheckResponse / RateLimitStateDto)
// in the ai-gateway-plugin-server source.
func TestRealAllowWireShapeDecodes(t *testing.T) {
	srv := rawUpstream(t, `{
	  "allowed": true,
	  "status_code": 200,
	  "tenant_id": "11377",
	  "rate_limits": [
	    {"scope":"key","dimension":"rpm","window":"minute","limit":600,"remaining":599,"reset":41},
	    {"scope":"key","dimension":"tpm","window":"minute","limit":1000000,"remaining":999900,"reset":41}
	  ]
	}`, 200)
	defer srv.Close()

	rec := serve(t, srv)
	if rec.Code != 200 {
		t.Fatalf("got status %d, want 200 — the real allow body must decode", rec.Code)
	}
	if got := rec.Header().Get("X-Tenant-ID"); got != "11377" {
		t.Fatalf("got X-Tenant-ID %q, want %q", got, "11377")
	}
	// Header naming matches the Kong plugin byte for byte
	// (kong-plugin/kong/plugins/ai-gateway/handler.lua build_ratelimit_headers):
	//   X-RateLimit-{Limit,Remaining,Reset}-{scope}-{dimension}-{window}
	for h, want := range map[string]string{
		"X-RateLimit-Limit-key-rpm-minute":     "600",
		"X-RateLimit-Remaining-key-rpm-minute": "599",
		"X-RateLimit-Reset-key-rpm-minute":     "41",
		"X-RateLimit-Limit-key-tpm-minute":     "1000000",
		"X-RateLimit-Remaining-key-tpm-minute": "999900",
	} {
		if got := rec.Header().Get(h); got != want {
			t.Errorf("got %s=%q, want %q", h, got, want)
		}
	}
	// Nothing is exhausted, so there must be no Retry-After.
	if got := rec.Header().Get("Retry-After"); got != "" {
		t.Errorf("unexpected Retry-After %q on a non-exhausted allow", got)
	}
}

// An exhausted window gates the retry. Kong sets Retry-After to the LONGEST
// reset among exhausted windows; parity matters because clients back off on it.
func TestExhaustedWindowSetsRetryAfter(t *testing.T) {
	srv := rawUpstream(t, `{
	  "allowed": false,
	  "status_code": 429,
	  "stage": "ratelimit",
	  "reason": "rpm limit exceeded",
	  "tenant_id": "11377",
	  "rate_limits": [
	    {"scope":"key","dimension":"rpm","window":"minute","limit":600,"remaining":0,"reset":17},
	    {"scope":"key","dimension":"tpm","window":"hour","limit":100,"remaining":0,"reset":900},
	    {"scope":"key","dimension":"tpm","window":"minute","limit":1000,"remaining":500,"reset":9999}
	  ]
	}`, 200)
	defer srv.Close()

	rec := serve(t, srv)
	if rec.Code != 429 {
		t.Fatalf("got status %d, want 429", rec.Code)
	}
	// 900 (exhausted) beats 17 (exhausted); 9999 is NOT exhausted and must be
	// ignored, otherwise a healthy long window would inflate the backoff.
	if got := rec.Header().Get("Retry-After"); got != "900" {
		t.Fatalf("got Retry-After %q, want %q", got, "900")
	}
}

// A deny must still surface rate-limit state — that is how a 429 tells the
// client what it hit.
func TestRateLimitHeadersEmittedOnDenyToo(t *testing.T) {
	srv := rawUpstream(t, `{"allowed":false,"status_code":429,
	  "rate_limits":[{"scope":"tenant","dimension":"rpm","window":"5minute","limit":10,"remaining":0,"reset":30}]}`, 200)
	defer srv.Close()

	rec := serve(t, srv)
	if got := rec.Header().Get("X-RateLimit-Remaining-tenant-rpm-5minute"); got != "0" {
		t.Fatalf("got X-RateLimit-Remaining-tenant-rpm-5minute %q, want %q", got, "0")
	}
}

// An absent/empty rate_limits array must not break the allow path, and a
// malformed rate_limits value must still fail CLOSED rather than be ignored.
func TestAllowWithoutRateLimits(t *testing.T) {
	srv := rawUpstream(t, `{"allowed":true,"status_code":200,"tenant_id":"11377"}`, 200)
	defer srv.Close()

	if rec := serve(t, srv); rec.Code != 200 {
		t.Fatalf("got status %d, want 200", rec.Code)
	}
}

func TestMalformedRateLimitsFailsClosed(t *testing.T) {
	srv := rawUpstream(t, `{"allowed":true,"rate_limits":"not-an-array"}`, 200)
	defer srv.Close()

	if rec := serve(t, srv); rec.Code != 503 {
		t.Fatalf("got status %d, want 503 (a body we cannot fully decode is not a trustworthy allow)", rec.Code)
	}
}

// An allow with no tenant must not emit an empty X-Tenant-ID, which downstream
// would otherwise see as a real (blank) tenant.
func TestAllowWithoutTenantOmitsTenantHeader(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": true}, 200)
	defer srv.Close()

	rec := serve(t, srv)
	if rec.Code != 200 {
		t.Fatalf("got status %d, want 200", rec.Code)
	}
	if _, ok := rec.Header()["X-Tenant-Id"]; ok {
		t.Fatalf("X-Tenant-ID should be absent, got %q", rec.Header().Get("X-Tenant-ID"))
	}
}

// The decision is only as good as the request we send: wrong path, dropped
// body, or dropped auth headers would make the policy server judge the wrong
// thing while still returning a confident 200.
func TestForwardsPathBodyAndHeadersToUpstream(t *testing.T) {
	var (
		gotPath, gotMethod, gotBody, gotCT string
		gotHeaders                         http.Header
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		gotPath, gotMethod, gotBody = r.URL.Path, r.Method, string(b)
		gotCT = r.Header.Get("Content-Type")
		gotHeaders = r.Header.Clone()
		json.NewEncoder(w).Encode(map[string]any{"allowed": true})
	}))
	defer srv.Close()

	req := httptest.NewRequest("POST", "/check", strings.NewReader(`{"model":"gpt-4"}`))
	req.Header.Set("Authorization", "Bearer tok")
	req.Header.Set("X-Model", "gpt-4")
	req.Header.Set("X-Estimated-Tokens", "1234")
	req.Header.Set("X-Request-Id", "req-1")
	req.Header.Set("X-Should-Not-Forward", "nope")

	newHandler(srv.URL).ServeHTTP(httptest.NewRecorder(), req)

	if gotMethod != "POST" {
		t.Fatalf("got method %q, want POST", gotMethod)
	}
	if gotPath != "/v1/check" {
		t.Fatalf("got path %q, want /v1/check", gotPath)
	}
	if gotBody != `{"model":"gpt-4"}` {
		t.Fatalf("got body %q, want the original request body", gotBody)
	}
	if gotCT != "application/json" {
		t.Fatalf("got Content-Type %q, want application/json", gotCT)
	}
	for h, want := range map[string]string{
		"Authorization":      "Bearer tok",
		"X-Model":            "gpt-4",
		"X-Estimated-Tokens": "1234",
		"X-Request-Id":       "req-1",
	} {
		if got := gotHeaders.Get(h); got != want {
			t.Fatalf("upstream got %s=%q, want %q", h, got, want)
		}
	}
	if got := gotHeaders.Get("X-Should-Not-Forward"); got != "" {
		t.Fatalf("unexpected header forwarded: X-Should-Not-Forward=%q", got)
	}
}

// Empty headers must not be forwarded as empty values.
func TestEmptyHeadersAreNotForwarded(t *testing.T) {
	var gotHeaders http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotHeaders = r.Header.Clone()
		json.NewEncoder(w).Encode(map[string]any{"allowed": true})
	}))
	defer srv.Close()

	req := httptest.NewRequest("POST", "/check", strings.NewReader("{}"))
	req.Header.Set("X-Model", "")

	newHandler(srv.URL).ServeHTTP(httptest.NewRecorder(), req)

	if _, ok := gotHeaders["X-Model"]; ok {
		t.Fatalf("empty X-Model should not be forwarded")
	}
}

// A slow upstream must not hang the data path forever; the client timeout
// fires and the request is denied.
func TestSlowUpstreamFailsClosed(t *testing.T) {
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-release:
		case <-r.Context().Done():
		}
	}))
	defer srv.Close()
	defer close(release)

	h := newHandler(srv.URL)
	req := httptest.NewRequest("POST", "/check", strings.NewReader("{}"))
	ctx, cancel := context.WithTimeout(req.Context(), 150*time.Millisecond)
	defer cancel()

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req.WithContext(ctx))

	if rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}
