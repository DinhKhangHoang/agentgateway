# Phase 5 — Kong vs agentgateway comparison and go/no-go

**Date:** 2026-07-25
**Scope:** design doc §11 Phase 5 — *"Replay captured Kong traffic against both gateways; compare
correctness, latency and cost. Produce a go/no-go recommendation on the §9 gaps."*

---

## 0. Verdict

**Go, with two conditions.**

Agentgateway reproduces the Kong contract for the 5-model slice on both the canonical and the
legacy URL surface, with authorization, guardrails, weighting, retry and metrics all verified
against real traffic. Nothing found in this phase is architecturally disqualifying.

The two conditions:

1. **Close the token-accounting gap before any tenant-visible traffic.** Until then every request
   debits a flat 100 tokens regardless of real usage. This is not a rounding error — a measured
   deepseek call used 142 tokens and a gemini call 176, so tenants are under-debited by roughly
   30–40% on short prompts and far more on long ones. Phase 6 is doing this.
2. **Decide explicitly what happens to the 10.2% of the Kong surface agentgateway cannot serve**
   (§2). This is a hard structural limit of the current agentgateway release, not a config error.

The comparison is **weaker than the design intended** and the reasons are in §1. Read that before
quoting any number here.

---

## 1. What this comparison is, and is not

The design assumed captured Kong traffic replayed against both gateways. Neither half was possible.

**There is no captured traffic.** The entire available Kong dataplane access log for
`user-11377-maas-v2` — 3457 lines, ~27h of pod uptime — contains **zero** requests to any LLM
surface. Only `/status` health checks and internet background scanning. This is an idle dev
gateway. So the corpus is *derived from configuration* instead: all 128 live Kong routes, expanded
to 254 concrete requests across 8 route types and 6 providers. That is stronger for coverage
(configuration enumerates every route that exists; a capture only shows the ones exercised) and
weaker for performance (no real prompt-size distribution, no concurrency, no cache warmth).

**The Kong arm was never run.** Driving Kong requires a tenant API key issued by the real registry
at `pub-iamapis.api-dev.vngcloud.tech`. Nothing in the cluster can issue one. The pilot arm runs
only because the pilot resolves keys against a **test double** we control
(`pilot/16-registry-stub.yaml`) that authorizes one synthetic key.

Consequences, stated plainly:

- **There is no measured Kong-vs-agentgateway latency delta in this document.** Any such comparison
  would be fabricated. Pilot-side latencies are reported as absolute numbers only.
- **The real registry integration remains unverified.** The authorization chain is proven from the
  plugin server inward; the leg from the plugin server out to the production registry is not.
- Cost comparison is likewise absent: with no Kong arm and no real traffic mix, a cost model would
  be assumption stacked on assumption.

What the phase *did* produce: a complete surface-coverage differential, a contract differential
derived from both systems' live configuration and source, a measured pilot arm, and one serious
latent bug (§5).

---

## 2. Surface coverage — the headline structural finding

Agentgateway performs body-model routing only on a **hardcoded, non-configurable** path list in
`crates/agentgateway/src/store/binds.rs:741` (`model_router_matches()`). Mapping Kong's live route
types onto it:

| Kong `route_type` | Requests in corpus | agentgateway canonical path | Served? |
|---|---:|---|---|
| `llm/v1/chat` | 134 | `/v1/chat/completions` | yes |
| `llm/v1/messages` | 32 | `/v1/messages` | yes |
| `llm/v1/embeddings` | 22 | `/v1/embeddings` | yes |
| `llm/v1/responses` | 20 | `/v1/responses` | yes |
| `llm/v1/images/generations` | 14 | `/v1/images/generations` | yes |
| `llm/v1/rerank` | 6 | `/v1/rerank` | yes |
| **`llm/v1/generateContent`** | **16** | — | **no** |
| **`llm/v1/completions`** | **10** | — | **no** |

**26 of 254 requests (10.2%) target surfaces with no agentgateway canonical equivalent.** The
legacy-URL workaround (`AgentgatewayBackend` on an arbitrary path) still serves these, but without
body-model routing — so weighted virtual models do not work there.

This is the gap the design recorded as #5 at severity *Low*. **That rating is wrong and should be
raised to Medium.** It was scoped as a cosmetic 400-check difference; it is in fact a tenth of the
live request surface losing a routing capability.

---

## 3. Measured pilot results

Corpus fired at the pilot, authenticated, both surfaces. 2026-07-25.

### Canonical surface (model from body)

| Status | Count | Meaning |
|---|---:|---|
| 200 | 6 | gemini-2.5-flash, deepseek-v4-pro |
| 400 | 2 | claude-sonnet-4-0 — upstream *"credit balance is too low"*; **routing proven** |
| 401 | 2 | gpt-4o — upstream *"Incorrect API key"* from api.openai.com; **routing proven** |
| 404 | 2 | deepseek on `/v1/messages` — not registered for that surface |
| 403 | 216 | ACL denial: models outside the synthetic key's `allowed_models` |

### Legacy Kong URL surface

| Status | Count | Meaning |
|---|---:|---|
| 200 | 7 | both `/maas/user-11374/...` and bare `/...` prefix forms |
| 400 / 401 | 4 | same two upstream account problems |
| 404 | 243 | models the pilot does not serve |

### Latency, successful calls only

| Surface | n | min | median | max |
|---|---:|---:|---:|---:|
| canonical | 6 | 0.788s | 1.059s | 1.165s |
| legacy | 7 | 0.678s | 1.016s | 1.347s |

Legacy and canonical are indistinguishable at this sample size, which is the useful result: the
compatibility surface costs nothing measurable.

### Two asymmetries worth recording

- `deepseek-v4-pro` on `/v1/messages` returns **404 canonical but 200 legacy**. The legacy path is
  an `AgentgatewayBackend` passthrough that never consults the model router, so it forwards a
  surface the canonical router refuses. Same client, same model, two different answers depending on
  URL shape.
- Unconfigured models fail **403 on canonical** but **404 on legacy**. Routing runs before
  authorization, so a legacy request that matches no route never reaches extAuth. Kong behaves the
  same way, so this is parity rather than regression — but it means the legacy surface reveals which
  models exist without authenticating.

### SSE baseline

Measured for Phase 6 regression testing (`pilot/verify/phase5/sse_timing.py`):

| Model | TTFT | chunks | gap p50 | gap max | total |
|---|---:|---:|---:|---:|---:|
| deepseek-v4-pro (300 tok) | 0.214s | 129 | 0.02ms | 452ms | 2.05s |
| gemini-2.5-flash (200 tok) | 1.353s | 4 | 0.22ms | 87.7ms | 1.44s |

Usage totals are present in the terminal SSE frame for both providers — which is what makes
response-side token true-up feasible at all.

---

## 4. Contract differential

Derived from Kong's live plugin configuration and agentgateway's source. Not speculative.

| Concern | Kong today | agentgateway pilot | Assessment |
|---|---|---|---|
| Guardrail config | 4 guard plugins, all `external_config: true` pointing at `placeholder.invalid` — **everything** injected per-tenant at runtime by the policy server | static per-model regex | **regression**, Phase 6 |
| Token accounting | pre-debit 100 at `/v1/check`, corrected at `/v1/usage` in log phase, `usage_timeout_ms: 2000` | pre-debit 100, **never corrected** | **regression**, Phase 6 |
| Request body cap | `max_request_body_size: 50000000` (50 MB); 10 MB on selfhost routes | `max_buffer_size` default **2 MiB** (`lib.rs:323`) — and the extAuth CEL references `request.body`, forcing buffering | **regression, 24×.** Raise `maxBufferSize` or large prompts that work today will fail |
| Retry | Kong retries per upstream config | works on status codes and CEL conditions, but **silently disabled above 64 KiB** request body (`httpproxy.rs:973`, logged at `debug!` only) | subtle regression; no warning surfaces it |
| Streaming | `response_streaming: allow` | verified end to end | parity |
| Auth failure semantics | plugin returns body-encoded verdict | adapter translates to status; fail-closed verified 401/403/503 | parity |
| Access log | `file-log` with auth headers explicitly masked, custom fields for tenant/user/check-latency | on by default; gen_ai token fields present | parity, richer |
| Tracing | core only | W3C traceparent available (not enabled — no collector in cluster) | improvement |
| TTFT / TPOT histograms | `ai_proxy_ttft_ms`, `ai_proxy_tpot_latency_ms` | `agentgateway_gen_ai_server_time_to_first_token`, `agentgateway_gen_ai_server_time_per_output_token` | **parity — the design said these were unavailable; they exist.** Verified live on the pilot `/metrics` (72 families). Correcting §7 of the design |

### An unrelated security observation about Kong

While reading Kong's configuration read-only, provider credentials are stored **inline in
KongPlugin CRs** as plaintext `config.auth.header_value`, including at least one HTTP Basic
credential for a self-hosted upstream. Anyone with `get kongplugin` in that namespace can read
every provider key. Out of scope for this pilot and **not touched**, but it should be raised
separately — the pilot keeps the equivalent values in Secrets.

---

## 5. The bug this phase found

Verifying the allow path required standing up the registry test double. The first authenticated
request returned **503, not 200**:

```
ext_authz fail-closed: decoding upstream decision:
json: cannot unmarshal array into Go struct field decision.rate_limits of type map[string]string
```

`pilot/adapter/main.go` decoded a `/v1/check` response contract that does not exist. It declared
`tenant` and `rate_limits map[string]string`; the real `CheckResponse` has `tenant_id` and an
**array** of `{scope,dimension,window,limit,remaining,reset}`.

Both fields are absent from a *denial* body. So the bug was unreachable on every deny path — and
every earlier test exercised only deny paths, because no key existed. The adapter's own unit tests
asserted the invented shape and passed. **This would have shipped and failed every allowed
request.**

Fixed against the Rust source, with `X-RateLimit-*` naming and `Retry-After` semantics now matching
Kong's `handler.lua`, plus a regression test carrying the real wire body.

The lesson generalises: *fail-closed systems verified only against failures are not verified.*

---

## 6. Re-assessment of the design's §9 gaps

| # | Gap | Design severity | Now | Basis |
|---|---|---|---|---|
| 1 | Per-tenant guardrail config | High | **High, confirmed** | All 4 Kong guard plugins are `external_config: true`. `/v1/check` already returns `KeyConfig.guardrails[model]` as typed directives — the contract exists and is unused |
| 2 | `/v1/usage` TPM true-up | High | **High, confirmed** | Measured: real usage 142–176 tokens vs flat 100 estimate |
| 3 | VNG IAM token exchange | Medium | **Closed** | CronJob refreshing on schedule; verified |
| 4 | Web search / server-tool loop | Medium | unchanged | No extension point; still deferred |
| 5 | `body.model` vs path-segment | Low | **Raise to Medium** | It is not a 400-check nicety — 10.2% of the surface loses model routing (§2) |
| 6 | TTFT/TPOT histograms | Low | **Closed — was never a gap** | `agentgateway_gen_ai_server_time_to_first_token` / `..._time_per_output_token` confirmed exported on the live pilot |
| 7 | Retry disabled above 64 KiB | Low | **Raise to Medium** | Confirmed hardcoded, and it fails silently at `debug!` level |
| **8** | **Request buffer 2 MiB vs Kong 50 MB** | *(new)* | **Medium** | 24× reduction, and extAuth's CEL forces buffering |

---

## 7. Recommendation

Proceed to Phase 6 and close gaps 1 and 2 — in progress via a single ext_proc service, chosen over
patching agentgateway.

Before any tenant-visible traffic, additionally:

1. Raise `maxBufferSize` on the listener to match Kong's 50 MB, or measure the real prompt-size
   distribution and set it deliberately. The current 2 MiB default is an unexamined inheritance.
2. Decide the disposition of `generateContent` and `/v1/completions` (§2): accept the loss of model
   routing on the legacy surface, or make `model_router_matches()` configurable upstream.
3. Obtain a real tenant API key and re-run `pilot/verify/phase5/replay.py` against **both**
   gateways. That is the only way the latency and cost half of this comparison gets done, and it
   is the largest remaining hole.
4. Raise the Kong plaintext-credential issue (§4) with whoever owns that namespace.

## 8. Reproducing

```sh
cd pilot/verify/phase5
# corpus (read-only Kong snapshots; do NOT commit them, they contain provider keys)
python3 build_corpus.py --routes /tmp/kong-routes.json --plugins /tmp/kong-plugins.json --out corpus.json
# pilot arm
python3 replay.py --corpus corpus.json --host 116.118.88.175.nip.io \
                  --surface legacy --key "$PILOT_API_KEY" --out pilot-legacy.json
# SSE realtime check
python3 sse_timing.py --host 116.118.88.175.nip.io --key "$PILOT_API_KEY" \
                      --model deepseek-v4-pro --runs 3 --label whatever
```

`PILOT_API_KEY` for the pilot arm:

```sh
export PILOT_API_KEY=$(kubectl --kubeconfig=/home/stackops/.kubeconfig/aigateway-dev.conf \
  -n user-11377-maas-v2-agw get secret pilot-test-apikey -o jsonpath='{.data.key}' | base64 -d)
```
