// Command registry-stub is a TEST DOUBLE for the tenant/API-key registry that
// ai-gateway-plugin-server resolves keys against.
//
// WHY IT EXISTS
// The pilot's plugin server validates every API key with
// `GET {APP_BACKEND_API_URL}/v1/keys/{sha256}` against a remote registry
// (pub-iamapis.api-dev.vngcloud.tech). No key is registered there for the
// pilot and none can be minted, so every request denies and the authenticated
// ALLOW path — the last unverified leg of the pilot — could not be exercised
// at all. This stub answers that one lookup for ONE synthetic key so the allow
// path becomes testable. It proves the plugin server's authn/acl/ratelimit
// stages and the whole downstream chain work; it proves NOTHING about the real
// registry integration.
//
// SECURITY — THE ONE PROPERTY THAT MATTERS
// The pilot gateway sits on a public LoadBalancer IP. A stub that authorised
// any key would make that gateway world-open. Therefore:
//   - exactly ONE key hash is accepted, pinned via ACCEPTED_KEY_SHA256;
//   - there is no wildcard, no "accept all", and no empty-means-allow;
//   - the process REFUSES TO START unless ACCEPTED_KEY_SHA256 is exactly 64
//     hex characters, so a blank or missing env var can never degrade into an
//     open oracle;
//   - the comparison is constant time (crypto/subtle);
//   - logs carry only the first 12 characters of a hash and never the inbound
//     bearer token.
//
// Do not deploy this outside the pilot namespace, and never in front of real
// tenant traffic.
//
// WIRE CONTRACT (verified against the Rust source, crates/contract/src/lib.rs
// and crates/server/src/infra/cache/client.rs):
//   - the base URL is joined by plain string concat after trimming trailing
//     slashes, so the request path is `{whatever prefix}/v1/keys/{sha256}`;
//     this server therefore matches on the `/v1/keys/{sha}` SUFFIX and does
//     not care what prefix the configured base URL carries;
//   - on a match: 200 with a KeyConfig JSON. `key_hash` must byte-equal the
//     requested hash or the server fails closed; `expires_at` and
//     `allowed_models` have NO serde default, so omitting either is a decode
//     error that surfaces as a 503;
//   - on no match: 404 with an empty body — the server negative-caches it for
//     5s and returns a clean 401 "unknown api key". ANY other status becomes a
//     503, so no other status may ever be emitted on the lookup path;
//   - APP_IAM_ENABLED stays true for production parity, so lookups arrive with
//     an `Authorization: Bearer <iam-token>` header. It is accepted and
//     ignored — validating it is not this double's job.
package main

import (
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

// keysMarker is the fixed tail of the lookup URL. Everything before it is the
// prefix carried by APP_BACKEND_API_URL and is deliberately not inspected.
const keysMarker = "/v1/keys/"

// shaLen is the length of a lowercase hex SHA-256. Anything else is rejected
// outright, both at startup and per request.
const shaLen = 64

// scopedLimits mirrors contract::ScopedLimits.
type scopedLimits struct {
	RPM map[string]uint64 `json:"rpm"`
	TPM map[string]uint64 `json:"tpm"`
}

// limitConfig mirrors contract::LimitConfig. Only per_key is populated; the
// other scopes carry #[serde(default)] upstream and "absent = no limit".
type limitConfig struct {
	PerKey scopedLimits `json:"per_key"`
}

// keyConfig mirrors contract::KeyConfig. Field tags are load bearing: the
// plugin server decodes this exact shape and fails closed on any mismatch.
//
// ExpiresAt is a *int64 with NO omitempty on purpose — the upstream field has
// no serde default, so the key must be PRESENT and is serialised as `null`
// ("never expires"). Dropping it turns every lookup into a 503.
type keyConfig struct {
	KeyHash       string      `json:"key_hash"`
	Active        bool        `json:"active"`
	ExpiresAt     *int64      `json:"expires_at"`
	TenantID      string      `json:"tenant_id"`
	AllowedModels []string    `json:"allowed_models"`
	Limits        limitConfig `json:"limits"`
}

// config is the validated runtime configuration. Constructing one is the only
// way to get a serving handler, so an invalid environment cannot be served.
type config struct {
	acceptedSHA   string // lowercase hex, exactly shaLen characters
	tenantID      string
	allowedModels []string
	rpmMinute     uint64
	tpmMinute     uint64
	port          string
}

// matches reports whether requested is THE accepted key hash.
//
// Comparison is case insensitive (callers always send lowercase, but a
// tolerant comparison costs nothing) and constant time. The explicit length
// check is a second belt: it makes "" and "*" structurally unable to match,
// independent of what ConstantTimeCompare does with mismatched lengths.
func (c *config) matches(requested string) bool {
	if len(requested) != shaLen {
		return false
	}
	got := strings.ToLower(requested)
	if !isHex(got) {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(got), []byte(c.acceptedSHA)) == 1
}

func isHex(s string) bool {
	for i := 0; i < len(s); i++ {
		c := s[i]
		if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
			return false
		}
	}
	return len(s) > 0
}

// shaPrefix returns the at-most-12-character log-safe prefix of a hash. A full
// sha256 must never reach the logs: it is key-equivalent material for an
// offline guesser and defeats the point of hashing the key at all.
func shaPrefix(s string) string {
	if len(s) > 12 {
		return s[:12]
	}
	return s
}

// loadConfig validates the environment and refuses to produce a config unless
// every security-relevant value is explicitly and correctly set. getenv is
// injected so this is testable without mutating process state.
func loadConfig(getenv func(string) string) (*config, error) {
	sha := strings.ToLower(strings.TrimSpace(getenv("ACCEPTED_KEY_SHA256")))
	if len(sha) != shaLen || !isHex(sha) {
		// Deliberately does not echo the offending value.
		return nil, fmt.Errorf(
			"ACCEPTED_KEY_SHA256 must be exactly %d hex characters (got %d); "+
				"refusing to start because an unpinned key hash would make this an accept-all oracle",
			shaLen, len(sha))
	}

	tenant := strings.TrimSpace(getenv("TENANT_ID"))
	if tenant == "" {
		return nil, errors.New("TENANT_ID is required")
	}

	var models []string
	for _, m := range strings.Split(getenv("ALLOWED_MODELS"), ",") {
		if m = strings.TrimSpace(m); m != "" {
			models = append(models, m)
		}
	}
	if len(models) == 0 {
		return nil, errors.New("ALLOWED_MODELS is required (comma-separated, at least one model)")
	}

	cfg := &config{
		acceptedSHA:   sha,
		tenantID:      tenant,
		allowedModels: models,
		rpmMinute:     uintEnv(getenv, "RPM_MINUTE", 600),
		tpmMinute:     uintEnv(getenv, "TPM_MINUTE", 1000000),
		port:          strings.TrimSpace(getenv("PORT")),
	}
	if cfg.port == "" {
		cfg.port = "8080"
	}
	return cfg, nil
}

func uintEnv(getenv func(string) string, name string, def uint64) uint64 {
	v := strings.TrimSpace(getenv(name))
	if v == "" {
		return def
	}
	n, err := strconv.ParseUint(v, 10, 64)
	if err != nil || n == 0 {
		return def
	}
	return n
}

// extractSHA pulls the hash out of a lookup path, tolerating any prefix and an
// optional trailing slash. ok is false when this is not a lookup path at all.
func extractSHA(path string) (string, bool) {
	p := strings.TrimSuffix(path, "/")
	i := strings.LastIndex(p, keysMarker)
	if i < 0 {
		return "", false
	}
	sha := p[i+len(keysMarker):]
	if sha == "" || strings.Contains(sha, "/") {
		return "", false
	}
	return sha, true
}

// newHandler builds the serving handler. A plain HandlerFunc is used rather
// than http.ServeMux so path handling is fully explicit: no implicit trailing
// slash redirects, no surprise 301s in front of an authorization lookup.
func newHandler(cfg *config, lg *log.Logger) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/healthz" {
			w.WriteHeader(http.StatusOK)
			return
		}

		sha, ok := extractSHA(r.URL.Path)
		if !ok {
			// Not a lookup URL. 404 is also the correct answer for the plugin
			// server if it ever asked here, so this is safe either way.
			w.WriteHeader(http.StatusNotFound)
			return
		}

		if r.Method != http.MethodGet {
			lg.Printf("lookup rejected: method=%s sha=%s...", r.Method, shaPrefix(sha))
			w.Header().Set("Allow", http.MethodGet)
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}

		// The inbound Authorization bearer is accepted and IGNORED by design
		// (see the package comment). It is never read and never logged.
		if !cfg.matches(sha) {
			lg.Printf("lookup miss: sha=%s...", shaPrefix(sha))
			// Empty body: the server negative-caches this for 5s and turns it
			// into a clean 401 "unknown api key".
			w.WriteHeader(http.StatusNotFound)
			return
		}

		lg.Printf("lookup hit: sha=%s... tenant=%s", shaPrefix(sha), cfg.tenantID)
		body, err := json.Marshal(keyConfig{
			// Echo the LOWERCASE hash. The server compares this byte-for-byte
			// against what it asked for and fails closed on any difference.
			KeyHash:       strings.ToLower(sha),
			Active:        true,
			ExpiresAt:     nil, // present-and-null == never expires
			TenantID:      cfg.tenantID,
			AllowedModels: cfg.allowedModels,
			Limits: limitConfig{PerKey: scopedLimits{
				RPM: map[string]uint64{"minute": cfg.rpmMinute},
				TPM: map[string]uint64{"minute": cfg.tpmMinute},
			}},
		})
		if err != nil {
			// Unreachable for this fixed shape, but a 500 here would become a
			// 503 at the gateway, which is the correct fail-closed outcome.
			lg.Printf("lookup encode error: sha=%s...: %v", shaPrefix(sha), err)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write(body)
	})
}

func main() {
	lg := log.New(os.Stderr, "", log.LstdFlags|log.LUTC)

	cfg, err := loadConfig(os.Getenv)
	if err != nil {
		lg.Fatalf("registry-stub refusing to start: %v", err)
	}

	lg.Printf("registry-stub (TEST DOUBLE) listening on :%s — accepting exactly ONE key hash %s..., tenant=%s, models=%v",
		cfg.port, shaPrefix(cfg.acceptedSHA), cfg.tenantID, cfg.allowedModels)

	srv := &http.Server{
		Addr:              ":" + cfg.port,
		Handler:           newHandler(cfg, lg),
		ReadHeaderTimeout: 5 * time.Second,
	}
	lg.Fatal(srv.ListenAndServe())
}
