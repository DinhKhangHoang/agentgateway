# Kong → agentgateway parity generator

Translates the live MaaS Kong surface into agentgateway CRs. It is a translator,
not a one-off script: the same tool must run against prod later.

```
python3 -m pytest test_generate.py -q          # no cluster needed
python3 generate.py --check-default-table ~/agentgateway
python3 generate.py --kubeconfig … --namespace user-11377-maas-v2 --out-dir out
```

`test_generate.py` builds `Row` objects from the two live path shapes and runs
`classify` → `group` → `emit_models` in-process; only `extract()` touches a
cluster. Each test names the behaviour it pins, and the two that guard past
defects — the per-model route table and the recursive feature scan — were
checked by reintroducing each defect and watching them fail.

## Route-type mapping — the surface exists, because we added it

Kong's `route_type` selects a wire format. Agentgateway's equivalent is
`llm::Policy::routes`, a **path-suffix → RouteType map** consulted by
`resolve_route` (`crates/agentgateway/src/llm/policy/mod.rs:617`), which walks the
map longest-suffix-first and falls back to the `"*"` wildcard.

The generator emits it as `spec.policies.routes` on every `AgentgatewayModel`.
That field did not exist when this file was first written; it was added to the
fork in `f83e591c` (`Routes map[string]RouteType` on `ModelPolicies`, plumbed
through `translateModelRouteAIPolicy` in
`controller/pkg/agentgateway/translator/model_collections.go:462`). The Rust side
needed no change — `convert_backend_ai_policy` already read `routes` off the
`BackendPolicySpec_Ai` proto the model carries.

Two measured facts from before that landed are worth keeping, because they say
where the map is *not* reachable from:

- `AgentgatewayPolicy` cannot name a model. The API server rejects it with
  *"targetRefs may only reference Gateway, HTTPRoute, GRPCRoute, ListenerSet,
  Service, AgentgatewayBackend, or InferencePool resources"*.
- A `routes` map also exists at `AgentgatewayPolicy.spec.backend.ai.routes`, but it
  attaches to a Gateway/HTTPRoute/Backend, and the model router is a *synthetic*
  backend built with `inline_policies: vec![]`
  (`Store::rebuild_model_router`, `crates/agentgateway/src/store/binds.rs:800`).
  A Gateway-targeted `backend.ai.routes` policy therefore reports
  `Accepted=True, Attached=True` and has no effect on model-routed traffic —
  confirmed by probe, and confirmed again by deleting the policy and observing no
  behaviour change.

The per-model field is the only one that works.

## Two rules that follow from how the map is assembled

**`spec.policies.routes` replaces the built-in table wholesale.** It is not
merged with `default_route_types()`
(`crates/agentgateway/src/llm/model_router.rs:55`) — a model that sets `routes`
resolves *only* what it lists, and everything else falls to whatever `"*"` it
declared. So the generator never emits a partial table: `DEFAULT_ROUTE_TABLE`
mirrors all 13 built-in entries verbatim, and the extra paths are added on top.

Keeping that mirror honest is enforced, not remembered:

```
python3 generate.py --check-default-table /path/to/agentgateway
```

parses `default_route_types()` out of the fork's Rust source and exits non-zero
on any difference, naming each one — an entry present upstream and missing here
is reported as *"would be DELETED from every emitted model"*, because under
replacement semantics that is exactly what a stale mirror does. Run it after
every rebase of the fork. A moved or renamed function, or a parse that yields
zero entries, is also an error: a checker that silently finds nothing and
reports "no drift" would be worse than no checker.

**`match.paths` is listener-wide but `policies.routes` is per-model.**
`Store::rebuild_model_router` (`crates/agentgateway/src/store/binds.rs:810-817`)
unions `spec.match.paths` across every model bound to the listener, so one model
opening `/v1/completions` makes that path reachable for *all* of them. The route
table is not unioned. Emitting `routes` only on the models that opened a path
therefore leaves every other model reachable there and resolving it through
`("*", Passthrough)` — routed but unparsed, which is the FR-7.1 miss this whole
section exists to close. The generator emits the **same** full table, built from
the union of every path opened anywhere in the corpus, on **every** model.

## What that buys on the two missing Kong route types

Kong has two `route_type` values with no entry in the default table:

| Kong `route_type` | canonical path | before | now |
|---|---|---|---|
| `llm/v1/completions` | `/v1/completions` | `*` → `Passthrough` | `Detect` |
| `llm/v1/generateContent` | `/v1/generateContent` | `*` → `Passthrough` | `Detect` |

`Passthrough` is not a metered route type. The handler is explicit
(`crates/agentgateway/src/proxy/httpproxy.rs`, the `RouteType::Passthrough |
RouteType::Realtime` arm): *"We do not need LLM policies nor token-based rate
limits, etc."* It forwards the body untranslated and extracts nothing, so a
`Passthrough` route reports no token usage — a direct miss on FR-7.1, one of the
two pass/fail gates in the `ai-gateway-spec` skill, for every request on those
two paths.

`Detect` forwards untranslated *and* extracts model, `stream`, `temperature`,
`top_p`, `max_tokens` and `seed` via `Request::to_llm_request`
(`crates/llm/src/types/detect.rs`). That is what these paths need and what they
now get. Proven in-cluster: `gen_ai.*` fields appear on the access log line for
both paths where they were absent under `Passthrough`
(`pilot/verify/phase7/README.md`). Note the scope of that claim — it shows the
requests are *parsed*, which is the precondition for metering; measuring actual
token counts needs a successful upstream response and real credentials.

**The cost, which the generator must report:** `Detect`'s
`supports_prompt_guard()` is false and its `get_messages()` is
`unimplemented!()`, so prompt guard silently does not run. The check is against
the *resolved route type*, and the table is shared by the whole listener — so
opening these paths disables guardrails on them for **every** model on the
listener, not only the ones that asked. `report.md` records this under
`## Paths on which guardrails silently do not run`.

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

The replacement semantics are worth restating here, since they are the opposite
of what this file used to say: `merge_llm_policies`
(`crates/agentgateway/src/store/binds.rs:493`) takes the preferred `routes` map
**whole** when it is non-empty. Adding one key does not extend the defaults, it
discards them — which is why `DEFAULT_ROUTE_TABLE` in `generate.py` restates all
13 built-in entries.

Suffix precedence within a map is pinned by
`sorted_routes_prefer_specific_path_over_wildcard` and
`sorted_routes_prefer_longer_key_regardless_of_insert_order` in
`crates/agentgateway/src/llm/policy/tests.rs`: a longer key beats both the `"*"`
wildcard and its own shorter suffixes, independent of insertion order.

## The guardrail trap, now that Detect is reachable

`Detect` **disables prompt guard**, and the generator now emits `Detect`, so
this is live rather than hypothetical: `get_messages()` is `unimplemented!()`
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
facts are visible at once: the `Detect`-mapped surface, and the models named in
tenant guardrail config. That is this generator, and it emits the section into
`report.md`.

The heading is `## Paths on which guardrails silently do not run`, not "models".
Scoping it to models would understate it: the route table is unioned onto the
listener, the guard check is against the *resolved route type*, so a `Detect`
path disables prompt guard for **every** model bound to that listener — 162 of
them, not the 18 that opened the paths.
