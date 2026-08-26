# agentgateway scorecard — measured, 2026-08-25/26

The `EVALUATION.md` §2 candidate column, filled in from probes. **Scope: the
maas-v2 model-routed surface only.** Not prod-v1.

Build under test: `agentgateway:maas-v2-parity-dcb5b524`
(`v1.4.0-beta.1-20-gdcb5b524`), which carries the configurable retry cap. Local
probes ran against that image in a docker bridge network with three mock
upstream tiers; cluster probes ran against the same image deployed as
`maas-v2-agw` in `user-11377-maas-v2-agw`. Commits `52afb9e4` and `f83e591c`
landed after the image was built and are on no path these probes exercise.

**UNVERIFIED means not measured.** It is never an inference from source-reading.
Per the skill's evidence rules, every ✅/❌ below cites a probe; anything resting
on a source read alone is marked UNVERIFIED even where the source is clear.

## Functional

| FR | Requirement | Candidate | Evidence |
|---|---|---|---|
| **6.1** | **External pre-request authorizer** | ✅ **gate closed** | P8 |
| **7.8** | **Post-response usage reporting** | ⚠️ ✅ normal path, ❌ abort path | P8 control (usage present); P6 `usage_reported_on_aborted_request=false` |
| 1.1–1.3 | Model-name routing, aliases, route scale | ⚠️ routing ✅, scale UNVERIFIED | P1/P8 control route by body `model`; ~5,000-route scale not tested |
| 2.1–2.3 | Format translation both directions, streaming + non | ✅ for tested shapes | P1 (non-stream), P3 (SSE) |
| 2.6 | Server-side tool augmentation (web search) | ❌ **no equivalent** | no augmentation path exists |
| 2.6a | Augmentation request-transparent | N-A | P13 — no subject |
| 2.6b | Augmentation covers every route_type | N-A | no subject |
| 2.6c | Augmentation surfaces upstream failure | N-A | P16 — no subject |
| **2.7** | **Deterministic upstream serialization** | ✅ | **P12** — 18 captures, 3 distinct OS processes, one body sha256, tool-call `arguments` verbatim |
| 2.8 | Idle-stream keepalive | ❌ | **P15** — no SSE keepalive knob exists (only TCP `SO_KEEPALIVE`); 7.96 s client gap with zero heartbeat frames |
| **2.9** | **Prompt head append-only across turns** | ✅ | **P18** — 22 turns, leading system block byte-identical, `mid_conversation_system_messages_hoisted=false`, shared prefix grows monotonically 833→11,828 B |
| 3.1–3.2 | Pool distribution, weights | UNVERIFIED | priority groups configured, not probed |
| 3.3 | Session affinity | ❌ | none found |
| 3.4 | Load-aware selection | UNVERIFIED | P2C in source, not probed |
| 3.5 | Capacity/concurrency caps | ❌ | none found |
| 4.1 | Retry on TCP/DNS failure | UNVERIFIED | not probed separately |
| 4.2 | Retry on HTTP status, non-streaming | ✅ | P2 — tier0 1 hit, tier1 1 hit, client 200 |
| **4.3** | **Retry on HTTP status, STREAMING** | ✅ **native, on by default** | **P3** — fresh state, tier0 attempted then tier1 streams, client never sees the 503 |
| 4.4 | Ordered fallback pools | ✅ | P4 — 3 upstream hits for 1 client request; P14 — mechanism's own log line + `x-retry-attempt` present |
| 4.5 | Cooldown for failed targets | UNVERIFIED | eviction exists (P3 needed fresh state), not measured directly |
| 4.6 | Cooldown from provider rate-limit headers | UNVERIFIED | in source, not probed |
| 4.7 | Bounded attempts | ✅ | P7 — exactly 3 attempts, 0.11 s, no hang |
| 4.8 | Pool-exhaustion reject + `Retry-After` | ❌ | **P7** — `retry_after_header: None`; the last upstream's 503 is relayed rather than a gateway-authored reject |
| **4.9** | **Retry not silently disabled by body size** | ✅ **closed** | **P5** — retry at 1K/40K/60K/70K/100K/1M/**50M**. Control arm without `maxReplayBytes`: 60K retries, **70K does not** (503, one upstream hit) |
| 5.1–5.3 | Active + passive health, shared | UNVERIFIED | |
| 5.4 | DNS on the retry path | UNVERIFIED | |
| 6.2–6.8 | Identity, hashing, ACL, per-target creds | ⚠️ partial | ext-auth returns `X-Tenant-ID`/`X-Api-Key-Sha`; detail UNVERIFIED |
| 6.9 | Route-scoped policy context to authorizer | UNVERIFIED | |
| 7.1–7.7 | TPM+RPM, multi-window, atomic, fail-closed | ⚠️ fail-closed ✅, rest UNVERIFIED | P8; counters live in Tier 2 and survive the swap |
| **7.10** | **Metering covers bypass/hijack paths** | N-A | P17 — no bypass path. **Not** a clean bill: see FR-7.8 abort hole |
| 8.1–8.5 | Logs, metrics, failover observability, cost | ⚠️ partial | P6 access-log line and `llm::cost` policy event observed |
| 8.6 | Upstream attribution on every path | UNVERIFIED | |
| 8.7 | Ext-auth latency a first-class log field | UNVERIFIED | |
| 9.7 | Fail-fast on invalid config | ✅ | P11 — refuses to boot and names the faulting field |
| 9.x | Drain longer than longest stream | ✅ | P10 |

## Non-functional

| NFR | Target | Candidate | Evidence |
|---|---|---|---|
| 1.1 | `/check` p99 < 50 ms | UNVERIFIED | denials returned in 64–243 ms end-to-end incl. TLS+WAN; not a `/check` measurement |
| 2.1 | ~1,000 rps / ~60k concurrent streams | UNVERIFIED | no load test run |
| 2.3 | ~80 KB per held stream | UNVERIFIED | |
| 3.2 | Fail closed, no fail-open mode | ✅ | **P8** — valid key + zero authorizer endpoints ⇒ 403 in 0.07 s |
| 3.5 | Drain > longest stream | ✅ | **P10** — 860 frames, clean `[DONE]`, curl exit 0 across a pod replacement |
| 4.1 | Atomic counters | UNVERIFIED | Tier 3 unchanged by the swap |
| 4.5 | Byte determinism across processes | ✅ | **P12** |
| 5.1 | No raw credentials anywhere | ⚠️ | agentgateway uses `secretRef`; **Kong side has a second plaintext key location** — `config.transform.request.add.headers` carries an `Authorization` header on `byok-11374-gemini-2-5-flash` |
| 6.1 | Every retry/failover/reject observable, silence detectable | ⚠️ | P14 confirms the signal exists; silence-detection UNVERIFIED |
| 8.5 | Extension identifiers scoped, collisions fail loudly | UNVERIFIED | |

## What the probes changed about the picture

Three cells moved that source-reading had called differently:

1. **FR-2.7 byte determinism was an open question and is now a PASS.** Whether
   Rust's serializer is key-order-stable across processes had not been
   established. P12 says yes — observed stable over 18 captures spanning three
   distinct OS processes. That is an observation, not a guarantee: nothing in the
   code pins map iteration order the way Kong's `canonical_json` does.
2. **FR-2.9 is a PASS, and it is the defect we had to fix on the Kong side.**
   Agentgateway renders mid-conversation `role: system` messages in place instead
   of hoisting them into the prompt head, so the prefix stays append-only.
3. **FR-7.8 is not the clean pass the gate check suggested.** Usage is reported
   on the normal path but not on an aborted stream. Kong covers that path.
