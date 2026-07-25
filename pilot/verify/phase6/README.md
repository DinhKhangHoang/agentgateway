# Phase 6 — ext_proc: per-tenant guardrails + TPM true-up

One ext_proc service (`pilot/extproc`, deployed by `pilot/17-extproc.yaml`) closes
the pilot's last two gaps against production Kong. The acceptance criterion that
mattered most was **SSE must stay realtime**, so that is measured first.

Raw data: `sse-measurements.jsonl`, produced by
`pilot/verify/phase5/sse_timing.py`.

---

## 1. SSE realtime — the acceptance criterion

Every response chunk round-trips gateway -> pilot-extproc -> gateway before the
client sees it. `observability_mode` is hardcoded `false` in agentgateway, so
this is unavoidable, not a design choice. The question is only whether that hop
is free.

The headline comparison is the matched **n=8** pair, taken back to back by
deleting and re-applying the policy between them. The n=3/n=5 rows are the
earlier passes, kept because they were taken at different points in the rollout.

| run | model | n | ttft | chunks | gap_p50 | gap_max | total |
|---|---|---|---|---|---|---|---|
| Phase-5 baseline (no ext_proc) | deepseek-v4-pro | 3 | 0.214s | 129 | 0.02ms | 452ms | 2.05s |
| same-session control (no ext_proc) | deepseek-v4-pro | 3 | 0.263s | 127 | 0.02ms | 478ms | 2.58s |
| ext_proc active | deepseek-v4-pro | 3 | 0.244s | 146 | 0.02ms | 504ms | 2.59s |
| ext_proc + guardrail directive | deepseek-v4-pro | 3 | 0.246s | 122 | 0.02ms | 623ms | 2.07s |
| **matched control (no ext_proc)** | deepseek-v4-pro | **8** | **0.267s** | **126** | **0.02ms** | **480ms** | **2.01s** |
| **matched, ext_proc ACTIVE** | deepseek-v4-pro | **8** | **0.231s** | **132** | **0.02ms** | **474ms** | **2.00s** |
| Phase-5 baseline (no ext_proc) | gemini-2.5-flash | 5 | 1.353s | 4 | 0.22ms | 87.7ms | 1.44s |
| same-session control (no ext_proc) | gemini-2.5-flash | 5 | 1.390s | 4 | 1.72ms | 113ms | 1.51s |
| ext_proc active | gemini-2.5-flash | 5 | 1.276s | 4 | 3.82ms | 101ms | 1.40s |
| **ext_proc + guardrail** | gemini-2.5-flash | **9** | **1.268s** | **4** | **7.32ms** | **108ms** | **1.42s** |

A same-session control was taken immediately before enabling the policy, because
comparing against numbers measured on a different day would confound provider
variance with our effect. The control reproduced the Phase-5 baseline, so the
baseline is sound.

### One transient we chased down rather than averaged away

The n=8 run taken **~3 minutes after a `rollout restart`** of `pilot-extproc`
read `ttft_median = 0.457s`, with individual runs as bad as 1.35s and an earlier
n=3 pass reading 2.58s. That is a genuine ~190ms median regression against the
control, and it is recorded in `sse-measurements.jsonl` as
`postdeploy-deepseek` and `postdeploy-deepseek-n8` rather than discarded.

It does not persist. Re-running the identical measurement once the pods were
warm gave `ttft_median = 0.231s` (`extproc-deepseek-n8-rerun`), slightly better
than the matched control. Throughout the transient `gap_p50` stayed at 0.02ms
and `chunks` at ~125, so nothing was ever being buffered — only the *first*
token was late.

**Operational consequence, and it is real:** for the first minutes after a
`pilot-extproc` pod starts, TTFT can spike to ~1.3s. Contributing factors are a
cold HTTP connection pool from the new pod to the plugin server (the `/v1/check`
call is synchronous on the request path), a cold `KeyConfig` cache in the plugin
server if it also restarted, and the `go run` container competing for its 1-core
limit right after compiling. A rolling restart is therefore not free, which is
another reason `replicas: 2` matters. This would largely disappear with a
prebuilt image instead of `go run`.

**Verdict: no steady-state regression.** The failure signature to watch for was
`chunks` collapsing toward 1 and `ttft` rising toward `total`. Neither happened:

* `chunks` on deepseek stayed at 121-163 against a 126 control (the count tracks
  how many tokens the model happened to emit, which varies per run; it did not
  collapse). `gap_p50` is **0.02ms, byte-identical to the control** — chunks are
  still arriving back-to-back.
* `ttft` did not rise on either model in steady state: 0.231s vs a 0.267s
  control on deepseek (n=8 each), 1.268s vs 1.390s on gemini. Both are
  marginally *lower* with ext_proc active, which is run-to-run noise, not an
  improvement claim. See the warm-up transient above for the one case where it
  did rise.
* `gap_max` tracks the model's own thinking pauses (deepseek emits reasoning
  tokens in bursts) and moved within its existing spread.

**One number that looks bad and is not:** gemini's `gap_p50` reads 1.72ms
control vs 7.32ms after. Gemini emits only **4 chunks**, so `gap_p50` is the
median of *three* samples. Across the n=9 run the per-run values were
1.2, 1.4, 1.4, 5.1, 7.3, 28.3, 32.7, 32.8, 51.2 ms — a 40x spread on identical
config. It is noise, not signal. The stable indicators on gemini (`chunks`,
`ttft`, `gap_max`, `total`) are all flat or slightly better.

### Why it stays realtime

`pilot/extproc/processor.go` `onResponseBody` Sends the chunk back
byte-identical **before** anything else — no parse, no lock, no I/O ahead of the
Send. The usage scan runs after, bounded and reusing its buffers. `/v1/usage` is
fire-and-forget on its own goroutine.

`TestResponseBodyChunkIsForwardedWithoutWaitingForMoreChunks` feeds exactly one
chunk and then blocks forever in `Recv`. This test was **verified to fail** by
temporarily mutating the processor to accumulate-and-flush:

```
--- FAIL: TestResponseBodyChunkIsForwardedWithoutWaitingForMoreChunks (0.50s)
    processor_test.go:260: no ProcessingResponse within 500ms
--- FAIL: TestResponseBodyChunksAreForwardedUnmodifiedAndInOrder (2.00s)
```

so it is a real guard, not decoration.

---

## 2. TPM true-up (Gap 2)

Kong pre-debits `estimated_tokens=100` at `/v1/check` and corrects it at
`/v1/usage`. The pilot never corrected, so tenants were under-debited by
`actual - 100` on every request.

Controlled before/after on the live Redis counter
(`rl:{11377}:key:<sha256>:tpm:minute:<bucket>` in `pilot-dragonfly`), same
bucket before and after:

```
bucket                 = 29749779
TPM before             = 788
actual tokens          = 416
TPM after              = 1204   (same bucket)
delta observed         = 416     <-- the real usage
flat estimate would be = 100
```

The counter moved by **416, the true token count**, not by the flat 100. Before
this phase it would have moved by 100 and the tenant would have been
under-debited by 316 tokens on that single request.

Corroborating processor logs across streaming and non-streaming:

```
"TPM true-up posted" model=deepseek-v4-pro delta_tokens=-29 total_tokens=71  chunks=1   <- non-streaming, a REFUND
"TPM true-up posted" model=deepseek-v4-pro delta_tokens=43  total_tokens=143 chunks=28  <- streaming
"TPM true-up posted" model=deepseek-v4-pro delta_tokens=87  total_tokens=187 chunks=50  <- streaming
"TPM true-up posted" model=gemini-2.5-flash delta_tokens=53 total_tokens=153 chunks=5
```

Both directions work: `delta_tokens=-29` is an over-estimate refund, proving the
correction is signed and not just an add-on.

Streaming usage is read from the terminal SSE `data:` frame, parsed
incrementally with a bounded carry so a frame split across chunk boundaries
still resolves (`TestUsageFromSSEFrameSplitAcrossChunks` splits the real
deepseek terminal frame at *every* byte offset).

---

## 3. Per-tenant guardrails (Gap 1)

A `keyword` directive was added to the registry stub's `KeyConfig.guardrails`
for `deepseek-v4-pro` only (`pilot/16-registry-stub.yaml`, `GUARDRAILS_JSON`).
`deepseek-v4-pro` was chosen because it has **no** static `promptGuard`, so a
403 there can only have come from the new per-tenant path.

| # | request | result |
|---|---|---|
| 1 | deepseek + prompt containing `vng-secret-project` | **403** with the tenant's custom message |
| 2 | deepseek + "What is the capital of France?" | **200** |
| 3 | **gemini** + the same `vng-secret-project` prompt | **200** — proves the directive is per-MODEL, not global |
| 4 | deepseek + keyword prompt with `"stream": true` | **403** — not bypassable by asking to stream |
| 5 | gemini + `my SSN is 123-45-6789` (static promptGuard) | **403** — no regression in the existing mechanism |

Case 1 response body, showing the operator-supplied message and code carried
through from the directive `config`:

```json
{"error":{"code":"guardrail_keyword_tenant_policy",
          "message":"Blocked by gateway guardrail: this API key is not permitted to reference that project.",
          "param":null,"type":"invalid_request_error"}}
```

The envelope matches the static promptGuard's byte-for-byte in shape, so clients
see one contract.

### The provider is never contacted on a block

Gateway access log for the keyword denial:

```
http.path=/v1/chat/completions http.status=403 reason=DirectResponse duration=5ms
```

There is **no `endpoint=` field** — the upstream was never dialed. This is why
the processor defers its `request_headers` ack (see the package comment in
`pilot/extproc/config.go`): sending the `ImmediateResponse` as the *first*
message makes agentgateway's main request loop return it before
`mutate_request` dispatches the backend call. Acking headers eagerly would still
return 403 to the client, but only after the provider had been dialed.

For contrast, the static regex guardrail on the same gateway logs
`endpoint=generativelanguage.googleapis.com:443 reason=Guardrail duration=20ms`.

---

## 4. Auth and blast radius

* Unauthenticated `POST /v1/chat/completions` -> **401**. Fail-closed extAuth is
  unaffected by adding ext_proc.
* Non-streaming request with ext_proc active -> **200** in 1.81s.
* The Kong production namespace `user-11377-maas-v2` was **not modified**. Its
  newest resource dates from 2026-07-24; nothing was created there on 2026-07-25.
  Only `get` was ever issued against it.

---

## 5. Reproducing

```bash
KC=/home/stackops/.kubeconfig/aigateway-dev.conf
NS=user-11377-maas-v2-agw

# unit tests: 28 functions / 73 cases, plus 38 in the registry stub
(cd pilot/extproc      && go test ./...)
(cd pilot/registry-stub && go test ./...)

# SSE timing (never print the key)
K=$(kubectl --kubeconfig=$KC -n $NS get secret pilot-test-apikey -o jsonpath='{.data.key}' | base64 -d)
python3 pilot/verify/phase5/sse_timing.py --host 116.118.88.175.nip.io --key "$K" \
  --model deepseek-v4-pro --max-tokens 300 --runs 3 --label after
```

---

## 6. Known limits

* The `keyword` directive is enforced from an **inline** keyword list. Production
  Kong's `keyword-guard-request` delegates to an external keyword microservice
  that does not exist in the pilot; a directive carrying only
  `external_keyword_service_url` is therefore treated as unsupported and
  **denied** by default rather than silently passed. See the long comment in
  `pilot/extproc/guardrail.go`.
* `prompt`, `presidio` and `llama` directives are likewise unsupported and deny
  by default (`UNSUPPORTED_GUARDRAIL_ACTION=deny`). Setting `skip` allows them
  through but logs loudly at WARN with the directive kind.
* `failureMode: FailClosed` means the pilot returns errors, not unguarded
  answers, if `pilot-extproc` is down. Hence `replicas: 2`. Rationale is argued
  in `pilot/17-extproc.yaml`.
* A `pilot-extproc` restart costs a few minutes of elevated TTFT (up to ~1.3s on
  deepseek) before the pods warm up; see section 1. Streaming itself is
  unaffected during the transient.
* Response-side guardrails are not implemented — only the request prompt is
  inspected, matching what Kong's `*-guard-request` plugins do.
* The plugin server caches `KeyConfig` for 600s, so a guardrail change in the
  registry stub needs that TTL or a plugin-server restart to take effect.
