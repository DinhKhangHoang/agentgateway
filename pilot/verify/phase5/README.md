# Phase 5 — Kong vs agentgateway comparison harness

## What changed from the design

The design (§11) says: *"Replay captured Kong traffic against both gateways."*

**There is no captured traffic.** The entire available Kong dataplane access log for
`user-11377-maas-v2` — 3457 lines covering ~27h of pod uptime — contains zero requests to any
LLM surface. Only `/status` health checks and internet background scanning. This is an idle
dev gateway, not a loaded one.

So the corpus is **derived from configuration** instead of observed. The trade is worth naming:

- **Weaker** for latency and cost. There is no real prompt-size distribution, no concurrency,
  no cache-warmth profile. Any latency number from this harness is a synthetic-load number.
- **Stronger** for surface coverage. Configuration enumerates every route that *exists*;
  a capture only shows the ones that happened to be exercised. All 128 live Kong routes are
  represented, including surfaces no client has touched in days.

Every corpus entry corresponds to a real, programmed Kong route.

## Regenerating the corpus

Two read-only snapshots of the Kong namespace:

```sh
K=/home/stackops/.kubeconfig/aigateway-dev.conf
kubectl --kubeconfig=$K -n user-11377-maas-v2 get httproute  -o json > /tmp/kong-routes.json
kubectl --kubeconfig=$K -n user-11377-maas-v2 get kongplugin -o json > /tmp/kong-plugins.json

python3 build_corpus.py --routes /tmp/kong-routes.json --plugins /tmp/kong-plugins.json \
                       --out corpus.json
```

`build_corpus.py` reads only `model`, `provider` and `route_type` from each plugin. It never
touches the `auth` block, which holds provider API keys in plaintext. **Do not commit the two
snapshot files** — they contain those keys. `corpus.json` is safe and is committed.

## Running an arm

```sh
# pilot, canonical surface
python3 replay.py --corpus corpus.json --host 116.118.88.175.nip.io \
                  --surface canonical --key "$PILOT_API_KEY" --out pilot-canonical.json

# pilot, legacy Kong URLs — the arm that proves a client needs no change
python3 replay.py --corpus corpus.json --host 116.118.88.175.nip.io \
                  --surface legacy --key "$PILOT_API_KEY" --out pilot-legacy.json
```

`--dry-run` prints the plan without a single network call. `--limit` and `--route-type` keep a
smoke run to a handful of requests. Every corpus body caps output tokens, but a full 254-request
run still spends real provider credit on every model the pilot can reach — start with `--limit`.

The harness is deliberately gateway-agnostic: only `--host`, `--surface` and `--key` differ
between arms, and nothing in it knows which gateway it is talking to, so neither arm can be
accidentally favoured. TLS verification is off for **both** arms (the pilot presents a
self-signed cert on a nip.io host); doing it for one arm only would make the latency comparison
dishonest.

## The Kong arm is not runnable yet

Running the Kong side needs a tenant API key issued by the real registry at
`pub-iamapis.api-dev.vngcloud.tech`. None is obtainable from the cluster. The pilot side is
runnable because the pilot resolves keys against a **test double** (`pilot/16-registry-stub.yaml`)
that authorizes one synthetic key — a substitution that is valid for the pilot only and proves
nothing about the real registry integration.

Until a real key exists, the comparison is **contract-level plus a measured pilot arm**, not a
true A/B. See `agentgateway-maas-v2-phase5-comparison.md`.
