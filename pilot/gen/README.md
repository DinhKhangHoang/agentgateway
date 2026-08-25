# Kong → agentgateway parity generator

Translates the live MaaS Kong surface into agentgateway CRs. It is a translator,
not a one-off script: the same tool must run against prod later.

## Route-type mapping — the surface does not exist yet

Kong's `route_type` selects a wire format. Agentgateway's equivalent is
`llm::Policy::routes`, a **path-suffix → RouteType map** consulted by
`resolve_route` (`crates/agentgateway/src/llm/policy/mod.rs:617`), which walks the
map longest-suffix-first and falls back to the `"*"` wildcard.

An earlier revision of this file claimed that map is reachable from an
`AgentgatewayModel` as `spec.policies.routes`. **That is wrong.** Measured
against the fork and the live dev cluster on 2026-08-25:

- `ModelPolicies` (`controller/api/v1alpha1/agentgateway/agentgateway_model_types.go:118`)
  has exactly eight fields — `transformations`, `authorization`, `auth`, `health`,
  `tls`, `tunnel`, `headers`, `promptGuard`. There is no `routes`.
- `AgentgatewayPolicy` cannot name a model. The API server rejects it with
  *"targetRefs may only reference Gateway, HTTPRoute, GRPCRoute, ListenerSet,
  Service, AgentgatewayBackend, or InferencePool resources"*.
- A `routes` map does exist at `AgentgatewayPolicy.spec.backend.ai.routes`, but it
  attaches to a Gateway/HTTPRoute/Backend, and the model router is a *synthetic*
  backend built with `inline_policies: vec![]`
  (`Store::rebuild_model_router`, `crates/agentgateway/src/store/binds.rs:800`).
  A Gateway-targeted `backend.ai.routes` policy therefore reports
  `Accepted=True, Attached=True` and has no effect on model-routed traffic —
  confirmed by probe, and confirmed again by deleting the policy and observing no
  behaviour change.

So today **every model-routed request resolves its route type from the built-in
default table alone** (`crates/agentgateway/src/llm/model_router.rs:55`), and that
table cannot be extended, narrowed, or overridden per model from any CRD.

## What that costs on the two missing Kong route types

Kong has two `route_type` values with no entry in the default table:

| Kong `route_type` | canonical path | resolved RouteType today |
|---|---|---|
| `llm/v1/completions` | `/v1/completions` | `*` → `Passthrough` |
| `llm/v1/generateContent` | `/v1/generateContent` | `*` → `Passthrough` |

`spec.match.paths` (the Task 5 field) makes those paths *route* — that half works,
and is proven in-cluster. But `Passthrough` is not a metered route type. The
handler is explicit (`crates/agentgateway/src/proxy/httpproxy.rs`, the
`RouteType::Passthrough | RouteType::Realtime` arm): *"We do not need LLM policies
nor token-based rate limits, etc."* It forwards the body untranslated and extracts
nothing.

**A `Passthrough` route reports no token usage.** That is a direct miss on FR-7.1,
one of the two pass/fail gates in the `ai-gateway-spec` skill, for every request on
those two paths. Detect — which forwards untranslated *and* extracts model,
`stream`, `temperature`, `top_p`, `max_tokens` and `seed` via
`Request::to_llm_request` (`crates/llm/src/types/detect.rs`) — is the route type
these paths need, and there is currently no way to ask for it.

Closing this needs a `routes` field on `ModelPolicies`, plumbed through
`translateModelRouteAIPolicy`
(`controller/pkg/agentgateway/translator/model_collections.go:462`) into the
`BackendPolicySpec_Ai` the model already carries. The Rust side needs no change:
`convert_backend_ai_policy` already reads `routes` off that proto message. Until
that lands, the generator must not emit route-type config — there is nowhere to
put it.

## The default table is fragile — a fixed bug worth remembering

`ModelRoute::from_xds` (`crates/agentgateway/src/types/agent_xds.rs:1527`) used
`.unwrap_or_else(default_route_types)`. Any model carrying an AI policy at all
therefore took the *other* branch and ended up with an **empty** `routes` map, at
which point `resolve_route` returned its hardcoded `RouteType::Completions` for
every path — embeddings, rerank, messages, responses, images and Vertex
`:rawPredict` all parsed and translated as OpenAI chat completions.

The controller triggers this whenever a model sets `spec.policies.transformations`,
because `translateModelRouteAIPolicy` returns a non-nil AI policy for that alone.
Since a model-name rewrite is exactly what Kong parity needs, this would have hit
nearly every generated model.

Measured on dev before the fix, model `deepseek-v4-pro` (whose concrete target
`deepseek-v4-pro-direct` sets a `model` transformation):

```
POST /v1/completions  {"model":..., "prompt":...}    → 503 failed to parse request: missing field `messages`
POST /v1/completions  {"model":..., "messages":[…]}  → 400 from DeepSeek: missing field `prompt`
```

Both prove `route_type == Completions` where the wildcard should have given
`Passthrough`. Fixed on `feat/maas-v2-parity`: an AI policy with an empty `routes`
map now inherits the default table, while one that defines its own `routes`
replaces it outright.

Note the replacement semantics, since they are the opposite of what this file used
to say: `merge_llm_policies` (`crates/agentgateway/src/store/binds.rs:493`) takes
the preferred `routes` map **whole** when it is non-empty. Adding one key does not
extend the defaults, it discards them. Any future `routes` surface must therefore
restate every suffix it still wants.

Suffix precedence within a map is pinned by
`sorted_routes_prefer_specific_path_over_wildcard` and
`sorted_routes_prefer_longer_key_regardless_of_insert_order` in
`crates/agentgateway/src/llm/policy/tests.rs`: a longer key beats both the `"*"`
wildcard and its own shorter suffixes, independent of insertion order.

## The guardrail trap, when Detect does become reachable

`Detect` **disables prompt guard**: `get_messages()` is `unimplemented!()`
(`crates/llm/src/types/detect.rs`, "prompt guard is disabled for detect"), and
`InputFormat::supports_prompt_guard()` returns `false`. A route mapped to `Detect`
cannot carry a guardrail webhook — the handler is never invoked.

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
facts are visible at once: the set of `Detect`-mapped models, and the set of models
named in tenant guardrail config. That is this generator. It **must** emit, into
`report.md`, the full list of `Detect`-mapped models under the heading
`## Models that cannot carry guardrails`.

Today that list is necessarily empty, because no model can be mapped to `Detect` at
all. The requirement stands for when the `routes` surface lands.
