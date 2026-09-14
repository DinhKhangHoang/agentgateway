# AC vs AG Workload-Class Segregation

This example shows how to segregate two distinct LLM workload classes —
**AI coding agents (AC)** and **async generation / batch (AG)** — onto
separate backend groups with independent health, timeout, and retry
policies.

## When to use this pattern

Use segregation when your workload classes have **opposite** performance
profiles that would interfere if they shared a queue:

| | **AC (AI coding)** | **AG (async batch)** |
|---|---|---|
| Prompt size | small–medium (few–30k tok) | large (30k–200k tok) |
| Output | tens–hundreds of tokens | hundreds–thousands |
| Latency SLO | TTFT p95 < 300 ms | throughput; TTFT lenient |
| Timeout | long (600s) — agentic sessions are multi-turn | short (120s) — batch jobs are bounded |
| Retry | yes (5xx) — transient farm blips | no — caller re-submits on failure |
| Health probe | every 30s, evict after 2 failures | every 10s, evict after 3 failures |

A 200k-token AG prefill would **head-of-line-block** autocomplete requests
if they shared a queue. Segregation keeps their timeouts, retry behavior,
and health thresholds independent.

## How it works

The config has two layers:

1. **Model definitions** (`llm.models[]`) — each model carries its own
   `health` policy with `activeProbe` and `eviction` settings. The model
   name in the client's request selects the backend group.

2. **Route policies** (`binds[].listeners[].routes[].policies`) — retry
   and timeout are route-level policies. Two routes on port 8080 match
   the `x-workload-class` header to apply the right retry/timeout per
   class.

### Health probing (G4)

The `activeProbe` block configures agentgateway's active health prober
(FR-5.1–5.3):

- **`interval`** — time between probe sweeps across all endpoints.
- **`timeout`** — per-probe connect+read timeout.
- **`consecutiveFailures`** — consecutive probe failures before eviction.

The prober sends a tiny request (`max_tokens: 1`) to each endpoint and
feeds the outcome into the same eviction machinery as real traffic. A
dead backend is evicted **before** the next real request hits it.

### Eviction

The `eviction` block controls what happens after an endpoint is marked
unhealthy:

- **`duration`** — how long the endpoint is removed from the active set.
- **`consecutiveFailures`** — consecutive unhealthy responses (from real
  traffic or probes) before eviction.
- **`restoreHealth`** — health score to restore when the endpoint returns
  from eviction (gradual recovery).

## Running the example

Set API keys for the two backend groups:

```bash
export AC_CODING_API_KEY=...
export AG_BATCH_API_KEY=...
```

Start agentgateway:

```bash
cargo run -- -f examples/ops-ac-vs-ag-segregation/config.yaml
```

Send a request to the AC model (via the LLM API on port 4000):

```bash
curl http://localhost:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "ac-coding",
    "messages": [{"role": "user", "content": "Complete this function"}]
  }'
```

Send a request to the AG model:

```bash
curl http://localhost:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "ag-batch",
    "messages": [{"role": "user", "content": "Generate a summary of this document"}]
  }'
```

## Adapting for production

- **Separate SGLang farms** — point each model at a different SGLang
  router with dedicated AC/AG worker pools. At large scale, use separate
  farms entirely (cleanest isolation).
- **K8s deployment** — replace the `binds` section with
  `AgentgatewayPolicy` CRDs that have `targetRefs` pointing at the
  HTTPRoute. See `controller/test/e2e/testdata/aibackend/setup.yaml`
  for the CRD pattern.
- **Capacity awareness** — add `concurrent` and `tpm` capacity gates per
  backend group so a saturated farm is skipped during load balancing.
- **Sticky affinity** — enable session-to-farm affinity so an identity's
  traffic stays on the farm whose prefix cache is warm (G6, forward-looking).
- **Failover** — configure a fallback backend group (e.g., an online
  provider) as last-resort overflow for each workload class.
