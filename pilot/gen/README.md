# Kong → agentgateway parity generator

Translates the live MaaS Kong surface into agentgateway CRs. It is a translator,
not a one-off script: the same tool must run against prod later.

## Route-type mapping the generator must emit

Kong's `route_type` selects a wire format. Agentgateway's equivalent is
`spec.policies.routes`, a **path-suffix → RouteType map**
(`crates/agentgateway/src/llm/policy/mod.rs:155`, exposed on the CRD as
`Routes map[string]RouteType`). The built-in table at
`crates/agentgateway/src/llm/model_router.rs:55` is only a *default*: adding a key
replaces that one suffix and leaves the remaining defaults in place, so never
restate the whole table.

Two Kong route types have no built-in entry:

| Kong `route_type` | `spec.policies.routes` entry | `spec.match.paths` entry (needs the Task 5 field) |
|---|---|---|
| `llm/v1/completions` | `"/v1/completions": Detect` | `/v1/completions` |
| `llm/v1/generateContent` | `"/v1/generateContent": Detect` | `/v1/generateContent` |

`Detect` passes the body through **untranslated** while still extracting what the
platform meters on. `Request::to_llm_request`
(`crates/llm/src/types/detect.rs`) reads the model from the body via
`lookups::MODEL = [["model"], ["message","model"]]`, plus `stream`,
`temperature`, `top_p`, `max_tokens` and `seed` for telemetry. Both of these
canonical paths carry a top-level `model` — Kong's own `ai-routing` plugin
requires one in order to route at all — so `Detect` delivers routing, metering
and telemetry with no Rust changes.

Suffix precedence is what makes this safe, and it is pinned by
`sorted_routes_prefer_specific_path_over_wildcard` and
`sorted_routes_prefer_longer_key_regardless_of_insert_order` in
`crates/agentgateway/src/llm/policy/tests.rs`. A longer key beats both the `"*"`
wildcard and its own shorter suffixes, independent of insertion order.

## The cost of `Detect`, stated plainly

`Detect` **disables prompt guard**: `get_messages()` is `unimplemented!()`
(`crates/llm/src/types/detect.rs`, "prompt guard is disabled for detect"). A route
mapped to `Detect` cannot carry a guardrail webhook — the handler is never
invoked.

Verified on the live dev-v2 config on 2026-08-25: none of the 6
`llm/v1/completions` plugins and none of the 8 `llm/v1/generateContent` plugins
carry any guardrail config. This platform keeps guardrails in Tier 2 `KeyConfig`,
not in the ai-proxy plugin, so nothing Kong does today is lost.

**But it is a live trap.** Tier-2 guardrails are keyed per api-key *and model*. If
a tenant's `KeyConfig.guardrails` names a model served by a `Detect` route, the
guard is configured, accepted, and silently never runs. Under `Detect` this is
unfixable by configuration.

The warning cannot live in the webhook handler — on a `Detect` route that handler
is never reached, which is precisely the failure. It has to be raised where both
facts are visible at once: the set of `Detect`-mapped models, and the set of
models named in tenant guardrail config. That is this generator. It **must** emit,
into `report.md`, the full list of `Detect`-mapped models under the heading
`## Models that cannot carry guardrails`.
