# Phase 7 probe — does `X-Api-Key-Sha` reach a `promptGuard` webhook?

Date: 2026-08-25
Cluster: `aigateway-dev`, namespace `user-11377-maas-v2-agw` (agentgateway pilot)
Gateway: `https://116.118.88.175.nip.io`
Dataplane image: `.../dev/agentgateway:usage-report-300d7906`

## Question

Per-tenant policy lives in an external service (Tier 2), so the proxy's route
count scales with *models*, not *tenants × models*. For a per-tenant guardrail
to work, a guardrail webhook must be able to tell **which API key** the request
came from. `X-Api-Key-Sha` is injected into the request by the ext-auth policy
(`pilot/08-policy-extauth.yaml`, `http.allowedResponseHeaders`).

Does that header reach a `promptGuard.request[].webhook`?

## Verdict: **PRESENT**

The header arrives, and it is the SHA-256 of the raw API key — a stable
per-key identifier the webhook can key policy on.

**Raw evidence** — one line per request, straight from
`kubectl -n user-11377-maas-v2-agw logs deployment/echo-webhook`:

```
{"path": "/request", "headers": {"x-api-key-sha": "6d9bd8efce25f410dd7eefac056077901cb225e172d63dfe5205be192e71f5b9", "x-probe-control": "probe-1", "content-type": "application/json", "host": "echo-webhook.user-11377-maas-v2-agw.svc.cluster.local:8099", "content-length": "54"}, "body_len": 54}
{"path": "/request", "headers": {"x-api-key-sha": "6d9bd8efce25f410dd7eefac056077901cb225e172d63dfe5205be192e71f5b9", "x-probe-control": "probe-2", "content-type": "application/json", "host": "echo-webhook.user-11377-maas-v2-agw.svc.cluster.local:8099", "content-length": "54"}, "body_len": 54}
{"path": "/request", "headers": {"x-api-key-sha": "6d9bd8efce25f410dd7eefac056077901cb225e172d63dfe5205be192e71f5b9", "x-probe-control": "probe-3", "content-type": "application/json", "host": "echo-webhook.user-11377-maas-v2-agw.svc.cluster.local:8099", "content-length": "54"}, "body_len": 54}
```

All three requests returned `200`. The webhook logged on every one, so the
guard demonstrably fired — a 200 alone would not have proved that.

Cross-check that the value really identifies the key (run without ever echoing
the key itself):

```
$ printf '%s' "$K" | sha256sum
6d9bd8efce25f410dd7eefac056077901cb225e172d63dfe5205be192e71f5b9
```

Byte-identical to the forwarded header.

### Consequence

**Task 3b is unnecessary.** Per-tenant guardrails work today with
`forwardHeaderMatches` and need no agentgateway code change. A webhook can
receive `x-api-key-sha` and look up per-tenant guardrail config on it.

## `forwardHeaderMatches`: which form the CRD accepts

The task draft proposed an existence-style match, `type: Exact, value: ""`.
**The CRD rejects it.** `value` is `required` with `minLength: 1`
(verified in `kubectl get crd agentgatewaymodels.agentgateway.dev`), so an
empty string cannot express "any value":

```
$ kubectl apply --dry-run=server -f probe-webhook-model.yaml   # with type: Exact, value: ""
The AgentgatewayModel "deepseek-v4-pro-direct" is invalid:
spec.policies.promptGuard.request[1].webhook.forwardHeaderMatches[0].value:
Invalid value: "": ... in body should be at least 1 chars long
```

**The accepted form is `type: RegularExpression, value: ".*"`.** Use that
wherever a header is to be forwarded regardless of its value:

```yaml
forwardHeaderMatches:
- name: X-Api-Key-Sha
  type: RegularExpression
  value: ".*"
```

Note the semantics: `forwardHeaderMatches` is a *filter*, not a *rename*. Only
headers that both appear on the incoming request and match are forwarded; the
webhook sees nothing else from the client request. That is why the probe added
a positive control (below) rather than relying on the absence of a header.

## Method

1. `echo-webhook.py` — a `BaseHTTPRequestHandler` that logs one JSON line per
   `POST` (path, lower-cased headers, body length) and always answers
   `{"action": {"pass": {}}}`. The guard runs `failureMode: FailClosed`, so a
   wrong response shape would have failed the request and produced a
   misleading result. It did not: all three probes returned 200.
2. Deployed as `deployment/echo-webhook` + `service/echo-webhook:8099` from a
   ConfigMap-mounted source on `python:3.12-slim`. (The pod CrashLoopBackOffs
   between `create deployment` and the volume `patch` — expected.)
3. `probe-webhook-model.yaml` re-applied `AgentgatewayModel/deepseek-v4-pro-direct`
   with the webhook appended as a **second** `promptGuard.request` entry.
   `kubectl apply` replaces `spec.policies` wholesale, so the live regex
   guardrail and the health/eviction policy are reproduced verbatim in that
   file. Confirmed still live mid-probe:

   ```
   $ curl ... -d '{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"tell me about VNG-Secret-Project"}],...}'
   {"error":{"code":"guardrail_keyword_tenant_policy", ...}}
   http_code=403
   ```
4. Three authorized requests, each carrying an extra `X-Probe-Control` header.
5. Teardown: deployment/service/configmap deleted, `pilot/06-model-deepseek.yaml`
   re-applied.

### Positive control

`X-Probe-Control: probe-N` was set by curl and added as a second
`forwardHeaderMatches` entry. It arrives in every log line. Had
`x-api-key-sha` been missing while `x-probe-control` arrived, that would have
been solid evidence the sha genuinely is not on the request at guard time —
rather than the forwarding machinery being misconfigured. Both arrived, so the
control merely corroborates.

## Corrections to the task text

1. **The curl in Step 4 targets a model that cannot be requested.**
   `deepseek-v4-pro-direct` is `visibility: Internal`; requesting it directly
   returns `404 {"error":{"code":"model_not_found"}}` before any guard runs.
   The webhook logged nothing. The guard must be reached through the **public
   virtual model** `deepseek-v4-pro`, which weight-splits 99:1 onto
   `-direct` / `-dashscope`. Attaching the webhook to the concrete `-direct`
   target is still correct (`policies` cannot coexist with `virtualModel`);
   only the *request* must name the virtual model. Note the 1% dashscope arm
   carries no webhook, so a single request is not a reliable probe — fire
   several.
2. **`type: Exact, value: ""` is rejected**, as the task text anticipated.
   `RegularExpression` / `.*` is the working form.

## Incidental finding: the CEL `headers` field Task 3b would have added

Task 3b was framed as plumbing a CEL `headers` field onto the webhook guard so
the sha could be derived from `request.headers["authorization"]`. Two things
are worth recording even though the task is now moot:

- The **Rust dataplane already implements it.** `Webhook.headers` and
  `apply_header_expressions()` landed upstream in `9a07fe01`
  ("feat(llm): configurable webhook guardrail headers and path via CEL",
  #2595), which is an ancestor of the deployed build `300d7906`.
- What is missing is only the **control-plane half**. `convert_webhook()` in
  `crates/agentgateway/src/types/agent_xds.rs` reads:

  ```rust
  // CEL header expressions are not yet exposed via the XDS API.
  headers: Default::default(),
  ```

  and the installed CRD's `webhook` object exposes only `backendRef`,
  `failureMode`, `forwardHeaderMatches` — no `headers`.

So a future Task 3b would be a proto-field + Go-API + translation change, not
a dataplane change. It is not needed for per-tenant guardrails.

## Files

- `echo-webhook.py` — the webhook test double.
- `probe-webhook-model.yaml` — the transient model spec. **Not part of the
  pilot manifest set**; kept for reproducibility. Re-applying it would
  re-attach the probe webhook to a live model.

## Parity build deployed to dev — 2026-08-25

`feat/maas-v2-parity` @ `dcb5b524` built and pushed as:

| image | tag | digest |
|---|---|---|
| `agentgateway` (dataplane) | `maas-v2-parity-dcb5b524` | `sha256:076f5d41…` |
| `agentgateway-controller` | `maas-v2-parity-dcb5b524` | `sha256:007c6612…` |

Both images are required together: the configurable-path change touches the
proto and the CRD, so a controller emitting `ModelRoute.Match.paths` needs a
dataplane that reads it. The dataplane tag is not set on a Deployment — it comes
from the controller's `AGW_PROXY_IMAGE_TAG` env, and the controller container is
named `controller`, not `agentgateway`.

Push credential: `60108-khanghd` from the cluster's `vcr-registry-secret`. The
local `81-aigateway` login authenticates but has zero actions on this repo and
401s on push — the pilot's earlier "the registry is pull-only" note was a
credential problem, not a registry limitation.

The two changed CRDs (`agentgatewaymodels`, and the policy CRD) were applied with
`--force-conflicts --field-manager=agentgateway-parity` because helm owns
`.spec.versions`. **A later `helm upgrade` of the CRD chart will conflict back**
and silently drop `match.paths`.

Post-deploy smoke, against the same model and body as the pre-deploy baseline:

```
POST /v1/chat/completions  model=deepseek-v4-pro
HTTP 200   usage 85 prompt / 10 completion / 95 total   1.63s  (baseline 2.28s)
```

Identical to baseline on every field but latency. Dataplane self-reports
`version: "v1.4.0-beta.1-20-gdcb5b524"`.

### Task 5 acceptance, and what it exposed

`spec.match.paths` works. `POST /v1/completions` went from `404 route not found`
to a real routed request that hit ext-auth (`403 model not allowed for this key`
on a disallowed model, i.e. still failing closed) and then reached the LLM
pipeline on an allowed one.

It also exposed two things the plan did not anticipate, both written up in
`../../gen/README.md`:

1. A bug — a model carrying any AI policy lost the entire default route-type
   table and parsed every path as chat completions. Fixed on the same branch.
2. A missing surface — there is no per-model `routes` field on any CRD, so
   `/v1/completions` can only resolve to `Passthrough`, which does no token
   metering at all. Making these paths route is not the same as making them
   meter.

## Probe suite run — 2026-08-25/26

`probes.py` + `mock_upstream.py` implement the `ai-gateway-spec/EVALUATION.md` §3
suite. Results are JSON under `results/`; the filled-in scorecard is
`SCORECARD.md`.

Harness shape: three mock upstream tiers as containers on a dedicated docker
bridge network, each able to return 200-with-SSE or a chosen error on demand and
each logging every hit, so probes assert **upstream hit counts** rather than
client status. The gateway runs as a fourth container on that network, published
to `127.0.0.1` only — never `--network host`, never `0.0.0.0`.

| Probe | Verdict | File |
|---|---|---|
| P1, P2, P3, P4, P11, P14 | PASS | `results/core.json` |
| P6, P7 | PARTIAL | `results/core.json` |
| P5 | PASS | `results/p5.json` |
| P12 | PASS | `results/p12.json` |
| P15 | N-A (mechanism absent) | `results/p15.json` |
| P18 | PASS | `results/p18.json` |
| P8, P10 | PASS | `results/cluster.json` |
| P13, P16, P17 | N-A (no augmentation path) | `results/cluster.json` |

### The three results worth reading directly

**P5 carries its own control arm**, which is what makes it conclusive. With
`maxReplayBytes` configured, retry is observed at every size from 1 KB to 50 MB.
With the retry policy left at its default, 60 KB retries and **70 KB does not** —
503 to the client, one upstream hit, no second attempt. The 64 KiB cliff is
reproduced and closed in the same run.

**P8 was run against a genuinely absent authorizer**, not a misconfigured one.
`ai-gateway-plugin-server` was scaled to zero and its endpoints confirmed empty;
a **valid** API key then got `403 external authorization failed` in 0.066 s.
Fail-closed holds. One honest gap: scaling to zero produces a connection failure,
not a hang, so a slow-authorizer **timeout** is UNVERIFIED as a distinct case.

**P12 crossed a real process boundary** — two concurrent containers plus a
restart, three distinct pids, 18 captures, one body sha256. A single-pod
determinism test would have passed regardless, which is why it was not run that
way.

### P13, P16, P17 were not run, and that is a gap rather than a pass

All three probe the augmentation / hijacked-response path. Agentgateway has no
such path, so there is no subject to probe. This is the same fact that makes the
parity generator refuse the 7 dev / 6 prod `web_search` routes. **A future reader
must not read an absent probe as a passing one.**

Note especially that P17's absence does not mean metering is complete. P6
measured `usage_reported_on_aborted_request = false` — a real metering hole on a
path that does exist, and one Kong covers.

### Deviations from the probe plan

- **P15 is N-A rather than FAIL.** The probe assumes an idle-stream keepalive
  exists and asserts its frames. A search of the binary's `--help`, the config
  schema at `dcb5b524`, and the CRD surface found only TCP `SO_KEEPALIVE`, no SSE
  heartbeat. The measurement stands as evidence of the gap: a 7.96 s client-side
  frame gap with zero frames injected, decoded stream byte-identical to baseline.
- **P7 is PARTIAL, not PASS.** Attempts are bounded (exactly 3) and it does not
  hang, so FR-4.7 passes; but there is no `Retry-After` and the client receives
  the last upstream's 503 relayed rather than a gateway-authored reject, so
  FR-4.8 fails.
- **P6 is PARTIAL.** The mid-stream failure is surfaced rather than laundered
  into a clean-looking 200 — which is the important half — but usage is not
  reported for the aborted request.

---

## Post-probe build: `maas-v2-parity-f83e591c` — two fork fixes verified in-cluster

Date: 2026-08-26. Cluster `aigateway-dev`, namespace `user-11377-maas-v2-agw`.

Images built and pushed after the probe run above (which was on `dcb5b524`):

| Component | Tag | Digest |
|---|---|---|
| controller | `.../dev/agentgateway-controller:maas-v2-parity-f83e591c` | `sha256:45921174af6eb3e776437de7afb7ca2c25afc0faf21900a84a3a3d46b330825f` |
| dataplane | `.../dev/agentgateway:maas-v2-parity-f83e591c` | `sha256:8e86d6d1d3cfca74a0ba84e044ef82dadbf08f96e69077d4a5e4f5d110555b4d` |

Rolled by patching `deploy/agentgateway` in `agentgateway-system`: container
`controller` image, and env `AGW_PROXY_IMAGE_TAG`. The dataplane Deployment
`maas-v2-agw` picks the new tag up on the controller's next reconcile — it
reported "successfully rolled out" on the *old* image for ~30 s first, so verify
by reading `.spec.template.spec.containers[0].image`, not by `rollout status`.

The regenerated `agentgateway.dev_agentgatewaymodels.yaml` CRD was applied with
`--server-side --force-conflicts --field-manager=agentgateway-parity` because
helm owns `.spec.versions`. **A later `helm upgrade` of the CRD chart will
conflict back over this and silently drop `spec.match.paths` and
`spec.policies.routes` from the schema.**

### Fix 1 — `ModelRoute::from_xds` no longer discards the default route table

`crates/agentgateway/src/types/agent_xds.rs` (commit `52afb9e4`). Before the fix,
*any* AI policy on a model — including `policies.transformations` alone — replaced
the whole default path→`RouteType` table with an empty map, so every path except
the ones the policy restated stopped resolving.

Subject: `deepseek-v4-pro-direct`, which carries
`transformations: [{field: model, expression: "'deepseek-v4-pro'"}]` and therefore
emits an `ai_policy`. `spec.match.paths: ["/v1/completions"]` was added to open the
path on the listener; no `policies.routes` set, so the default table must supply
the route type.

```
POST /v1/completions  {"model":"deepseek-v4-pro","prompt":"say hi","max_tokens":8}
→ 400 {"error":{"message":"The supported API model names are deepseek-v4-pro, ...,
        but you passed deepseek-v4-pro-direct."}}
```

Access log for that request:

```
route=internal/llm:request endpoint=api.deepseek.com:443 http.path=/v1/completions
http.status=400 protocol=llm duration=293ms
```

`endpoint=api.deepseek.com:443` is the assertion. The request reached the upstream
via the default table's `*` → `Passthrough` entry. Pre-fix the same request never
left the router: `503`, `missing field 'messages'`. The absence of any `gen_ai.*`
field on the line confirms `Passthrough` specifically — it does no metering — and
the upstream's complaint that it "passed deepseek-v4-pro-direct" confirms the body
went through verbatim, un-transformed, as `Passthrough` requires.

### Fix 2 — `ModelPolicies.Routes` makes the route table addressable per model

`controller/api/v1alpha1/agentgateway/agentgateway_model_types.go` +
`controller/pkg/agentgateway/translator/model_collections.go:462` (commit
`f83e591c`). Before this there was no surface at all: a Gateway-targeted
`AgentgatewayPolicy` with `backend.ai.routes` reported `Accepted=True,
Attached=True` and did nothing, because the model router is a synthetic backend
with `inline_policies: vec![]`.

Same model, same request, with `policies.routes` set — the built-in table
restated plus one changed entry, since the field replaces wholesale:

```yaml
policies:
  routes:
    "/v1/chat/completions": Completions
    "/v1/completions":      Detect      # <- the entry under test
    "/v1/messages":         Messages
    "/v1/responses":        Responses
    "/v1/embeddings":       Embeddings
    "/v1/rerank":           Rerank
    "*":                    Passthrough
```

```
route=internal/llm:request endpoint=api.deepseek.com:443 http.path=/v1/completions
http.status=400 protocol=llm gen_ai.operation.name=chat gen_ai.provider.name=openai
gen_ai.request.model=deepseek-v4-pro gen_ai.request.max_tokens=8 duration=185ms
```

Two independent signals that `Detect` actually took effect on that path:

1. The `gen_ai.*` fields appear. Under `Passthrough` (Fix 1's line, same path,
   same body) they are absent — the request is now parsed and metered.
2. `gen_ai.request.model=deepseek-v4-pro`, and the upstream error changed from
   *"you passed deepseek-v4-pro-direct"* to DeepSeek's own *"completions api is
   only available when using beta api"*. The CEL transformation ran, so the body
   was deserialized and re-serialized rather than relayed — which only a parsing
   route type does.

Checking the status code alone would have proved nothing here: both arms return
`400`. The evidence is which `400`.

### Revert

Both mutations were removed (`kubectl patch --type=json`, `remove` on
`/spec/match/paths` and `/spec/policies/routes`). The resulting `spec` compares
byte-identical to the pre-probe snapshot, `deepseek-v4-pro` (virtual) is
unmodified, `/v1/chat/completions` still returns `200`, and `/v1/completions` is
back to `404 route not found`.

A patch adding `policies` to the *virtual* model `deepseek-v4-pro` was rejected by
the CRD (`policies cannot be used with virtualModel`) and so left nothing to undo.
Policies — and therefore route tables — are a concrete-model surface only.
