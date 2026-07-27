# RETIRED — 2026-07-27

This service no longer runs. Its Deployment, both Services and its source
ConfigMap were deleted from `user-11377-maas-v2-agw`, and its manifest
(`pilot/17-extproc.yaml`) was removed from this tree so a bulk
`kubectl apply -f pilot/` cannot resurrect a `failureMode: FailClosed` policy
pointing at a Service that is gone — which would fail **all** gateway traffic
closed, not just the guardrail path.

The source is kept here on purpose: it is the reference for what was replaced,
and the rollback path if either replacement regresses.

## What replaced each half

It closed two gaps with one processor. They now live in two different places.

**TPM true-up** → agentgateway's native `usageReport` policy
(`pilot/19-policy-usagereport.yaml`) posting absolute counts to the plugin
server's `POST /v1/usage/native`, which derives `delta = total − estimated`.
Verified to reproduce this service's numbers exactly: `delta=-89 total=11
prompt=6 completion=5` for the same deepseek request.

**Per-key keyword guardrail** → native `promptGuard` on **both** concrete
deepseek targets in `pilot/06-model-deepseek.yaml`, with a body byte-identical
to `Violation.Body()`.

## Three things that were nearly missed

1. **`promptGuard` cannot go on the virtual model.** The CRD forbids `policies`
   alongside `virtualModel`, and `deepseek-v4-pro` is virtual. It has to be on
   `deepseek-v4-pro-direct` AND `deepseek-v4-pro-dashscope`. The split is a
   99:1 weighted dice roll, so guarding only the 99% arm leaves a bypass that
   fires about 1 request in 100 and passes every manual test.
2. **`regex.action` defaults to `Mask`.** Without `action: Reject` the term is
   rewritten and the request is forwarded with a 200 — a masking policy, not a
   blocking one.
3. **Case sensitivity.** `keywordConfig.CaseSensitive` defaults false and this
   matcher lower-cased both sides, so the port needs `(?i)` or
   `VNG-Secret-Project` would newly pass. Verified: mixed case still 403.

## The fidelity gap that remains

This service fetched `KeyConfig.guardrails[model]` **per key, at request time**.
`promptGuard` is static per-model config. It reproduces the pilot's only live
directive exactly, but it is not per-tenant: a second key with a different
keyword list would now get this list instead of its own.

**Revisit before onboarding a second tenant.** Per-key guardrails need either a
gateway feature that can consult an external policy service per request, or
this service back.
