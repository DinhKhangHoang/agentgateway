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
