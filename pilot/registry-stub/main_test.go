package main

import (
	"bytes"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// A realistic lookup key: 64 lowercase hex characters, as the plugin server
// always sends. Deliberately NOT a constant that also appears in a log format
// string, so the "never log a full hash" assertions are meaningful.
const (
	goodSHA  = "3b1f8c0a9d2e4f6071829304a5b6c7d8e9f0a1b2c3d4e5f60718293041526374"
	otherSHA = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
	prefix   = "/cloud-ai-platform-api"
)

func testEnv(overrides map[string]string) func(string) string {
	base := map[string]string{
		"ACCEPTED_KEY_SHA256": goodSHA,
		"TENANT_ID":           "11377",
		"ALLOWED_MODELS":      "gemini-2.5-flash,gpt-4o",
	}
	for k, v := range overrides {
		if v == "\x00" { // sentinel meaning "unset"
			delete(base, k)
			continue
		}
		base[k] = v
	}
	return func(k string) string { return base[k] }
}

func mustConfig(t *testing.T, overrides map[string]string) *config {
	t.Helper()
	cfg, err := loadConfig(testEnv(overrides))
	if err != nil {
		t.Fatalf("loadConfig: unexpected error: %v", err)
	}
	return cfg
}

// newTestHandler builds the handler and captures everything it logs so tests
// can assert on log hygiene.
func newTestHandler(t *testing.T, overrides map[string]string) (http.Handler, *bytes.Buffer) {
	t.Helper()
	var buf bytes.Buffer
	lg := log.New(&buf, "", 0)
	return newHandler(mustConfig(t, overrides), lg), &buf
}

func do(t *testing.T, h http.Handler, method, path string, hdr map[string]string) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, path, nil)
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

// ---------------------------------------------------------------------------
// Startup validation. A stub that starts without a pinned key hash would be an
// accept-all oracle in front of a public LoadBalancer, so these are the most
// important tests in the file.
// ---------------------------------------------------------------------------

func TestLoadConfigRefusesBadEnv(t *testing.T) {
	cases := []struct {
		name      string
		overrides map[string]string
	}{
		{"sha unset", map[string]string{"ACCEPTED_KEY_SHA256": "\x00"}},
		{"sha empty", map[string]string{"ACCEPTED_KEY_SHA256": ""}},
		{"sha whitespace", map[string]string{"ACCEPTED_KEY_SHA256": "   "}},
		{"sha too short", map[string]string{"ACCEPTED_KEY_SHA256": "abc123"}},
		{"sha 63 chars", map[string]string{"ACCEPTED_KEY_SHA256": goodSHA[:63]}},
		{"sha 65 chars", map[string]string{"ACCEPTED_KEY_SHA256": goodSHA + "a"}},
		{"sha non-hex", map[string]string{"ACCEPTED_KEY_SHA256": strings.Repeat("z", 64)}},
		{"sha wildcard", map[string]string{"ACCEPTED_KEY_SHA256": "*"}},
		{"tenant unset", map[string]string{"TENANT_ID": "\x00"}},
		{"tenant empty", map[string]string{"TENANT_ID": "  "}},
		{"models unset", map[string]string{"ALLOWED_MODELS": "\x00"}},
		{"models empty", map[string]string{"ALLOWED_MODELS": ""}},
		{"models only separators", map[string]string{"ALLOWED_MODELS": " , , "}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg, err := loadConfig(testEnv(tc.overrides))
			if err == nil {
				t.Fatalf("expected refusal to start, got config %+v", cfg)
			}
		})
	}
}

func TestLoadConfigAcceptsValidEnvAndNormalises(t *testing.T) {
	cfg, err := loadConfig(testEnv(map[string]string{
		"ACCEPTED_KEY_SHA256": "  " + strings.ToUpper(goodSHA) + "  ",
		"ALLOWED_MODELS":      " gemini-2.5-flash , gpt-4o ,, claude-sonnet-4-0 ",
		"TENANT_ID":           " 11377 ",
	}))
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	if cfg.acceptedSHA != goodSHA {
		t.Errorf("acceptedSHA = %q, want lowercase %q", cfg.acceptedSHA, goodSHA)
	}
	if cfg.tenantID != "11377" {
		t.Errorf("tenantID = %q, want %q", cfg.tenantID, "11377")
	}
	want := []string{"gemini-2.5-flash", "gpt-4o", "claude-sonnet-4-0"}
	if len(cfg.allowedModels) != len(want) {
		t.Fatalf("allowedModels = %v, want %v", cfg.allowedModels, want)
	}
	for i := range want {
		if cfg.allowedModels[i] != want[i] {
			t.Fatalf("allowedModels = %v, want %v", cfg.allowedModels, want)
		}
	}
}

// ---------------------------------------------------------------------------
// The hit path — the exact wire contract the Rust plugin server decodes.
// ---------------------------------------------------------------------------

func TestLookupHitReturnsFullContract(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	rec := do(t, h, http.MethodGet, prefix+"/v1/keys/"+goodSHA, nil)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (body %q)", rec.Code, rec.Body.String())
	}
	if ct := rec.Header().Get("Content-Type"); !strings.Contains(ct, "application/json") {
		t.Errorf("Content-Type = %q, want application/json", ct)
	}

	// Decode into a raw map first: the plugin server has NO serde default for
	// expires_at or allowed_models, so their mere PRESENCE is part of the
	// contract. A typed struct would silently paper over an omitted field.
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(rec.Body.Bytes(), &raw); err != nil {
		t.Fatalf("body is not JSON: %v (%q)", err, rec.Body.String())
	}
	for _, f := range []string{"key_hash", "active", "expires_at", "tenant_id", "allowed_models", "limits"} {
		if _, ok := raw[f]; !ok {
			t.Errorf("required field %q absent from response (decode error -> 503)", f)
		}
	}
	if got := string(raw["expires_at"]); got != "null" {
		t.Errorf("expires_at = %s, want null", got)
	}

	var kc struct {
		KeyHash       string   `json:"key_hash"`
		Active        bool     `json:"active"`
		TenantID      string   `json:"tenant_id"`
		AllowedModels []string `json:"allowed_models"`
		Limits        struct {
			PerKey struct {
				RPM map[string]uint64 `json:"rpm"`
				TPM map[string]uint64 `json:"tpm"`
			} `json:"per_key"`
		} `json:"limits"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &kc); err != nil {
		t.Fatalf("decode: %v", err)
	}
	// The server fails CLOSED unless key_hash byte-equals what it asked for.
	if kc.KeyHash != goodSHA {
		t.Errorf("key_hash = %q, want the requested %q", kc.KeyHash, goodSHA)
	}
	if !kc.Active {
		t.Error("active = false, want true")
	}
	if kc.TenantID != "11377" {
		t.Errorf("tenant_id = %q, want %q", kc.TenantID, "11377")
	}
	if len(kc.AllowedModels) != 2 || kc.AllowedModels[0] != "gemini-2.5-flash" {
		t.Errorf("allowed_models = %v", kc.AllowedModels)
	}
	if kc.Limits.PerKey.RPM["minute"] == 0 || kc.Limits.PerKey.TPM["minute"] == 0 {
		t.Errorf("limits.per_key not populated: %+v", kc.Limits)
	}
}

func TestLookupHitEchoesLowercaseForUppercaseRequest(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	rec := do(t, h, http.MethodGet, prefix+"/v1/keys/"+strings.ToUpper(goodSHA), nil)
	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}
	var kc struct {
		KeyHash string `json:"key_hash"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &kc); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if kc.KeyHash != goodSHA {
		t.Errorf("key_hash = %q, want lowercase %q", kc.KeyHash, goodSHA)
	}
}

func TestLookupHitWorksForAnyBaseURLPrefix(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	for _, p := range []string{
		"/v1/keys/" + goodSHA,                             // no prefix
		"/cloud-ai-platform-api/v1/keys/" + goodSHA,       // production-shaped prefix
		"/a/b/c/v1/keys/" + goodSHA,                       // deeper prefix
		"/cloud-ai-platform-api/v1/keys/" + goodSHA + "/", // trailing slash
	} {
		if rec := do(t, h, http.MethodGet, p, nil); rec.Code != http.StatusOK {
			t.Errorf("GET %s: status = %d, want 200", p, rec.Code)
		}
	}
}

// APP_IAM_ENABLED=true stays on for production parity, so every real lookup
// carries a bearer. The stub must accept it and must not try to validate it.
func TestLookupIgnoresAuthorizationHeader(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	for _, v := range []string{"", "Bearer not-a-real-token", "garbage"} {
		rec := do(t, h, http.MethodGet, prefix+"/v1/keys/"+goodSHA, map[string]string{"Authorization": v})
		if rec.Code != http.StatusOK {
			t.Errorf("Authorization=%q: status = %d, want 200", v, rec.Code)
		}
	}
}

// ---------------------------------------------------------------------------
// The miss path. Anything that is not THE one accepted hash must 404. There is
// no wildcard and no accept-all mode; this is what keeps the public pilot LB
// from being world-open.
// ---------------------------------------------------------------------------

func TestLookupMissReturns404EmptyBody(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	for _, sha := range []string{
		otherSHA,
		goodSHA[:63] + "0",      // one character off
		strings.Repeat("0", 64), // all zeroes
		"",                      // no hash at all
		"*",                     // wildcard attempt
		strings.Repeat("a", 32), // wrong length
		goodSHA + goodSHA,       // doubled
	} {
		rec := do(t, h, http.MethodGet, prefix+"/v1/keys/"+sha, nil)
		if rec.Code != http.StatusNotFound {
			t.Errorf("sha %q: status = %d, want 404", sha, rec.Code)
		}
		if body := rec.Body.String(); body != "" {
			t.Errorf("sha %q: body = %q, want empty", sha, body)
		}
	}
}

func TestUnknownPathReturns404(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	for _, p := range []string{"/", "/v1", "/v1/keys", "/v2/keys/" + goodSHA, "/keys/" + goodSHA} {
		if rec := do(t, h, http.MethodGet, p, nil); rec.Code != http.StatusNotFound {
			t.Errorf("GET %s: status = %d, want 404", p, rec.Code)
		}
	}
}

func TestWrongMethodReturns405(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	for _, m := range []string{http.MethodPost, http.MethodPut, http.MethodDelete, http.MethodPatch} {
		rec := do(t, h, m, prefix+"/v1/keys/"+goodSHA, nil)
		if rec.Code != http.StatusMethodNotAllowed {
			t.Errorf("%s: status = %d, want 405", m, rec.Code)
		}
	}
}

func TestHealthz(t *testing.T) {
	h, _ := newTestHandler(t, nil)
	if rec := do(t, h, http.MethodGet, "/healthz", nil); rec.Code != http.StatusOK {
		t.Errorf("status = %d, want 200", rec.Code)
	}
}

// ---------------------------------------------------------------------------
// Log hygiene. A full sha256 in a log line is key-equivalent material for an
// offline guesser and defeats the point of hashing; a leaked bearer is worse.
// ---------------------------------------------------------------------------

func TestLogsNeverContainFullHashOrBearer(t *testing.T) {
	h, buf := newTestHandler(t, nil)
	const token = "Bearer super-secret-iam-token-value"

	do(t, h, http.MethodGet, prefix+"/v1/keys/"+goodSHA, map[string]string{"Authorization": token})
	do(t, h, http.MethodGet, prefix+"/v1/keys/"+otherSHA, map[string]string{"Authorization": token})
	do(t, h, http.MethodPost, prefix+"/v1/keys/"+goodSHA, map[string]string{"Authorization": token})

	out := buf.String()
	if out == "" {
		t.Fatal("nothing was logged; every lookup must be logged hit/miss")
	}
	for _, secret := range []string{goodSHA, otherSHA, strings.ToUpper(goodSHA), token, "super-secret-iam-token-value"} {
		if strings.Contains(out, secret) {
			t.Errorf("log leaked %q\nlog was:\n%s", secret, out)
		}
	}
	// It must still be useful: the 12-char prefix and a hit/miss verdict.
	if !strings.Contains(out, goodSHA[:12]) {
		t.Errorf("log does not carry the 12-char sha prefix; log was:\n%s", out)
	}
	if !strings.Contains(out, "hit") || !strings.Contains(out, "miss") {
		t.Errorf("log does not record hit/miss; log was:\n%s", out)
	}
}

func TestShaPrefixNeverExceeds12Chars(t *testing.T) {
	for _, in := range []string{goodSHA, "abc", "", "0123456789abcdef"} {
		got := shaPrefix(in)
		if len(got) > 12 {
			t.Errorf("shaPrefix(%q) = %q, longer than 12", in, got)
		}
		if len(in) >= 12 && got != in[:12] {
			t.Errorf("shaPrefix(%q) = %q, want %q", in, got, in[:12])
		}
	}
}

// Guard against a future refactor reintroducing an "empty means allow all"
// bug: an empty requested hash must never match a configured hash.
func TestEmptyRequestNeverMatches(t *testing.T) {
	cfg := mustConfig(t, nil)
	if cfg.matches("") {
		t.Fatal("empty sha matched the configured key")
	}
	if cfg.matches("*") {
		t.Fatal("wildcard matched the configured key")
	}
	if !cfg.matches(goodSHA) || !cfg.matches(strings.ToUpper(goodSHA)) {
		t.Fatal("the configured key did not match itself")
	}
}

var _ io.Writer = (*bytes.Buffer)(nil)
