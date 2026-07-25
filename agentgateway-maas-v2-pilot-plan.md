# Agentgateway MaaS-v2 Pilot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up an agentgateway gateway in namespace `user-11377-maas-v2-agw` that accepts the same client requests as the Kong AI gateway in `user-11377-maas-v2`, for five representative models.

**Architecture:** A Gateway API `Gateway` on `gatewayClass: agentgateway`. Legacy Kong URLs are matched by 3 HTTPRoutes that `URLRewrite` to canonical endpoints; `AgentgatewayModel` CRs then resolve the model from `body.model` to a provider backend. Authorization delegates to the existing Rust `ai-gateway-plugin-server` (a pilot copy) through a thin Go ext_authz adapter that translates its always-200 contract into HTTP status codes.

**Tech Stack:** Kubernetes, Gateway API v1, agentgateway CRDs (`agentgateway.dev/v1alpha1`), Helm, Go 1.22+ (adapter), kubectl.

**Scope:** Phases 1–4 of `agentgateway-maas-v2-pilot-design.md`. Phase 5 (Kong comparison) and Phase 6 (conditional agentgateway code changes) are separate plans.

---

## Conventions Used Throughout

Every task assumes these are exported in your shell:

```bash
export KUBECONFIG=/home/stackops/.kubeconfig/aigateway-dev.conf
export SRC_NS=user-11377-maas-v2        # existing Kong namespace (READ ONLY)
export NS=user-11377-maas-v2-agw        # new pilot namespace
export AGW_REPO=/home/stackops/agentgateway
```

Two more are set partway through and needed by every task after them:

- `GW_HOST` — the Gateway's assigned address, exported in Task 4 Step 4.
  **Actual value from the executed run: `116.118.88.175.nip.io`**
- `PILOT_API_KEY` — an API key seeded into the pilot database, exported in Task 13 Step 2b.
  Tasks 14 onward assume it is set.

## Corrections Found During Execution

Three issues surfaced while running Phase 1. All are fixed in the task text below;
recorded here so a re-run from scratch does not rediscover them.

1. **The controller image tag must be pinned.** The chart's default renders
   `cr.agentgateway.dev/controller:v0.0.0-dev`, which does not exist in the registry and
   yields `ImagePullBackOff`. Use `v1.4.0-beta.1` — the earliest release containing the
   `AgentgatewayModel` API (commit `a1d420a3`, 2026-07-24) and the release matching the CRDs
   in this checkout. See Task 3.

2. **`agentgateway-system` must carry the discovery label.** `discoveryNamespaceSelectors`
   filters the controller's *informer cache*, not just which Gateways it adopts. Without the
   label on its own namespace, the controller cannot read its own `kgateway-xds-cert` secret
   and every Gateway reconcile fails with `xDS TLS secret ... not found` while the secret is
   plainly present. Symptom is `Accepted=False / InvalidParameters` retrying forever.
   See Task 3 Step 2b.

3. **The VNG LoadBalancer needs idle-timeout annotations.** Without
   `vks.vngcloud.vn/idle-timeout-client: "1200"`, long streaming generations are severed at
   the platform default. Mirrors the Kong dataplane ingress. Kong's
   `enable-proxy-protocol: "*"` is deliberately NOT mirrored — PROXY protocol requires
   matching listener configuration that this Gateway does not enable. See Task 4 Step 2.

**Never modify anything in `$SRC_NS`.** It is read-only for this plan — we only copy credential values out of it.

**Never paste a credential into a file.** Every secret is created with `kubectl create secret` reading from the live cluster. Manifests reference secrets by name only.

All manifests are written to `$AGW_REPO/pilot/` and committed. Create that directory in Task 1.

---

## File Structure

| File | Responsibility |
|---|---|
| `pilot/00-namespace.yaml` | Namespace |
| `pilot/01-gateway.yaml` | Gateway + TLS listener |
| `pilot/02-httproutes.yaml` | 3 legacy-path routes with URL rewrites |
| `pilot/03-model-openai.yaml` | BYOK OpenAI model |
| `pilot/04-model-gemini.yaml` | Gemini model (chat + generateContent) |
| `pilot/05-model-claude.yaml` | Anthropic model |
| `pilot/06-model-deepseek.yaml` | 2 internal + 1 weighted virtual model |
| `pilot/07-model-vllm.yaml` | Custom provider for aiplatform vLLM |
| `pilot/08-policy-extauth.yaml` | `AgentgatewayPolicy` with `traffic.extAuth` |
| `pilot/09-policy-observability.yaml` | metrics / tracing / access logs + PodMonitor |
| `pilot/10-adapter.yaml` | extauthz-adapter Deployment + Service |
| `pilot/11-plugin-server.yaml` | Pilot copy of ai-gateway-plugin-server |
| `pilot/12-iam-refresher.yaml` | CronJob refreshing the VNG IAM bearer token |
| `pilot/13-policy-resilience.yaml` | Retry + timeout policy |
| `pilot/verify/chat.sh` | Reusable chat-completions probe |
| `pilot/verify/acceptance.sh` | Full acceptance sweep (design §10) |
| `pilot/adapter/main.go` | Adapter implementation |
| `pilot/adapter/main_test.go` | Adapter tests |
| `pilot/adapter/Dockerfile` | Adapter image |
| `pilot/iam-refresher/main.go` | IAM token exchange |
| `pilot/iam-refresher/main_test.go` | IAM token exchange tests |

---

# Phase 1 — Foundation

### Task 1: Create the pilot namespace and manifest directory

**Files:**
- Create: `pilot/00-namespace.yaml`

- [ ] **Step 1: Verify the source namespace is reachable and note the Kong gateway hostname**

```bash
kubectl -n $SRC_NS get gateway 11377-maas-v2-gw \
  -o jsonpath='{.status.addresses[0].value}{"\n"}'
```

Expected: `122.201.13.113.nip.io`

- [ ] **Step 2: Verify the pilot namespace does not exist yet**

```bash
kubectl get ns $NS
```

Expected: FAIL with `Error from server (NotFound): namespaces "user-11377-maas-v2-agw" not found`

- [ ] **Step 3: Create the manifest directory and namespace manifest**

```bash
mkdir -p $AGW_REPO/pilot
```

`pilot/00-namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: user-11377-maas-v2-agw
  labels:
    app.kubernetes.io/part-of: maas-v2-agw-pilot
    agentgateway-discovery: "enabled"
```

- [ ] **Step 4: Apply and verify**

```bash
kubectl apply -f $AGW_REPO/pilot/00-namespace.yaml
kubectl get ns $NS -o jsonpath='{.status.phase}{"\n"}'
```

Expected: `Active`

- [ ] **Step 5: Commit**

```bash
cd $AGW_REPO && git add pilot/00-namespace.yaml
git commit -m "pilot: add maas-v2-agw namespace"
```

---

### Task 2: Install agentgateway CRDs

**Files:**
- None created; installs cluster-scoped CRDs from `controller/install/helm/agentgateway-crds`

- [ ] **Step 1: Verify the CRDs are not already present**

```bash
kubectl get crd agentgatewaymodels.agentgateway.dev
```

Expected: FAIL with `Error from server (NotFound)`

- [ ] **Step 2: Install the CRD chart from the repo checkout**

```bash
helm upgrade --install agentgateway-crds \
  $AGW_REPO/controller/install/helm/agentgateway-crds \
  --namespace agentgateway-system --create-namespace --wait
```

- [ ] **Step 3: Verify all four CRDs are established**

```bash
for c in agentgatewaymodels agentgatewaybackends agentgatewaypolicies agentgatewayparameters; do
  kubectl get crd $c.agentgateway.dev \
    -o jsonpath="{.metadata.name}{'\t'}{.status.conditions[?(@.type=='Established')].status}{'\n'}"
done
```

Expected: four lines, each ending in `True`

- [ ] **Step 4: Verify the API version matches what later tasks use**

```bash
kubectl explain agentgatewaymodel --api-version=agentgateway.dev/v1alpha1 | head -3
```

Expected: output begins with `KIND:       AgentgatewayModel` and `VERSION:    agentgateway.dev/v1alpha1`

- [ ] **Step 5: Commit (no files; record the install in the log)**

```bash
cd $AGW_REPO && git commit --allow-empty \
  -m "pilot: install agentgateway CRDs (agentgateway.dev/v1alpha1)"
```

---

### Task 3: Install the agentgateway controller

**Files:**
- Create: `pilot/values-controller.yaml`

- [ ] **Step 1: Write the controller values file**

`pilot/values-controller.yaml`:

```yaml
# Pin to the release matching the CRDs installed from this checkout.
# The chart default is a v0.0.0-dev placeholder that does NOT exist in the
# registry; without this the controller lands in ImagePullBackOff.
image:
  registry: cr.agentgateway.dev
  tag: v1.4.0-beta.1

# Restrict config discovery to the pilot namespace so the controller never
# reads or reconciles anything in the Kong namespace.
discoveryNamespaceSelectors:
  - matchLabels:
      agentgateway-discovery: "enabled"
```

- [ ] **Step 1b: Verify the pinned image actually exists before installing**

```bash
docker manifest inspect cr.agentgateway.dev/controller:v1.4.0-beta.1 >/dev/null \
  && echo EXISTS || echo "NOT FOUND — pick another tag"
```

Expected: `EXISTS`

- [ ] **Step 2: Install the controller**

```bash
helm upgrade --install agentgateway \
  $AGW_REPO/controller/install/helm/agentgateway \
  --namespace agentgateway-system \
  --values $AGW_REPO/pilot/values-controller.yaml \
  --wait --timeout 5m
```

- [ ] **Step 2b: Label the controller's own namespace so it can read its xDS cert**

`discoveryNamespaceSelectors` filters the controller's informer cache. Without this label the
controller cannot see `agentgateway-system/kgateway-xds-cert` — its own certificate — and every
Gateway reconcile fails with `xDS TLS secret ... not found` even though the secret exists.

```bash
kubectl label ns agentgateway-system agentgateway-discovery=enabled --overwrite
```

Expected: `namespace/agentgateway-system labeled`

- [ ] **Step 3: Verify the controller is running**

```bash
kubectl -n agentgateway-system get deploy -o wide
kubectl -n agentgateway-system rollout status deploy/agentgateway --timeout=120s
```

Expected: `deployment "agentgateway" successfully rolled out`

- [ ] **Step 4: Verify the GatewayClass was created and accepted**

```bash
kubectl get gatewayclass agentgateway \
  -o jsonpath="{.metadata.name}{'\t'}{.status.conditions[?(@.type=='Accepted')].status}{'\n'}"
```

Expected: `agentgateway	True`

- [ ] **Step 5: Confirm the Kong GatewayClass is untouched**

```bash
kubectl get gatewayclass
```

Expected: both `agentgateway` and `11377-maas-v2-gwclass` listed; Kong's unchanged.

- [ ] **Step 6: Commit**

```bash
cd $AGW_REPO && git add pilot/values-controller.yaml
git commit -m "pilot: install agentgateway controller scoped to pilot namespace"
```

---

### Task 4: Create the Gateway and verify the LoadBalancer

**Files:**
- Create: `pilot/01-gateway.yaml`

- [ ] **Step 1: Generate a self-signed certificate and load it into a Secret**

The hostname is not known until the LoadBalancer is assigned, so use a wildcard for `nip.io`.

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout /tmp/agw-tls.key -out /tmp/agw-tls.crt \
  -subj "/CN=*.nip.io" -addext "subjectAltName=DNS:*.nip.io"

kubectl -n $NS create secret tls maas-v2-agw-tls \
  --cert=/tmp/agw-tls.crt --key=/tmp/agw-tls.key

rm -f /tmp/agw-tls.key /tmp/agw-tls.crt
```

- [ ] **Step 2: Write the Gateway manifest**

`pilot/01-gateway.yaml`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: maas-v2-agw
  namespace: user-11377-maas-v2-agw
spec:
  gatewayClassName: agentgateway
  infrastructure:
    # Mirrors the Kong dataplane-ingress Service. idle-timeout-client is the
    # important one: LLM generations run for minutes and the VNG platform
    # default severs long streaming responses. Kong's enable-proxy-protocol
    # is deliberately NOT mirrored — it needs matching listener config.
    annotations:
      vks.vngcloud.vn/idle-timeout-client: "1200"
      vks.vngcloud.vn/idle-timeout-member: "1200"
      vks.vngcloud.vn/idle-timeout-connection: "20"
  listeners:
    - name: https
      protocol: HTTPS
      port: 443
      hostname: "*.nip.io"
      tls:
        mode: Terminate
        certificateRefs:
          - kind: Secret
            name: maas-v2-agw-tls
      allowedRoutes:
        namespaces:
          from: Same
```

- [ ] **Step 3: Apply and wait for the Gateway to be programmed**

```bash
kubectl apply -f $AGW_REPO/pilot/01-gateway.yaml
kubectl -n $NS wait --for=condition=Programmed gateway/maas-v2-agw --timeout=180s
```

Expected: `gateway.gateway.networking.k8s.io/maas-v2-agw condition met`

- [ ] **Step 4: Capture the assigned address**

```bash
kubectl -n $NS get gateway maas-v2-agw \
  -o jsonpath='{.status.addresses[0].value}{"\n"}'
```

Expected: an IP or `<ip>.nip.io` hostname. Record it — later tasks call it `$GW_HOST`.

```bash
export GW_HOST=$(kubectl -n $NS get gateway maas-v2-agw -o jsonpath='{.status.addresses[0].value}')
echo $GW_HOST
```

- [ ] **Step 5: Verify the data plane pod is running**

```bash
kubectl -n $NS get pods -l gateway.networking.k8s.io/gateway-name=maas-v2-agw
```

Expected: one pod, `Running`, `1/1`

- [ ] **Step 6: Verify TLS terminates (404 is the expected success signal — no routes yet)**

```bash
curl -sk -o /dev/null -w '%{http_code}\n' https://$GW_HOST/
```

Expected: `404` — TLS handshake succeeded and the proxy answered. A connection error means the listener is wrong.

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/01-gateway.yaml
git commit -m "pilot: add Gateway with HTTPS listener"
```

---

# Phase 2 — Models

### Task 5: Create provider credential Secrets from the live Kong config

**Files:**
- None (secrets are created imperatively; values never touch disk)

- [ ] **Step 1: Extract and create the OpenAI BYOK secret**

```bash
kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-byok-11374-openai-generative-model \
  -o jsonpath='{.config.auth.header_value}' \
  | sed 's/^Bearer //' \
  | kubectl -n $NS create secret generic openai-byok-11374 --from-file=key=/dev/stdin
```

- [ ] **Step 2: Extract and create the Gemini secret**

```bash
kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-gemini-2.5-flash-model \
  -o jsonpath='{.config.auth.header_value}' \
  | sed 's/^Bearer //' \
  | kubectl -n $NS create secret generic gemini-api-key --from-file=key=/dev/stdin
```

- [ ] **Step 3: Extract and create the Anthropic secret**

```bash
kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-claude-sonnet-4-0-model \
  -o jsonpath='{.config.auth.header_value}' \
  | kubectl -n $NS create secret generic anthropic-api-key --from-file=key=/dev/stdin
```

- [ ] **Step 4: Extract and create the two DeepSeek secrets**

```bash
kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-deepseek-v4-pro-model \
  -o jsonpath='{.config.targets[0].auth.header_value}' | sed 's/^Bearer //' \
  | kubectl -n $NS create secret generic deepseek-direct-key --from-file=key=/dev/stdin

kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-deepseek-v4-pro-model \
  -o jsonpath='{.config.targets[1].auth.header_value}' | sed 's/^Bearer //' \
  | kubectl -n $NS create secret generic deepseek-dashscope-key --from-file=key=/dev/stdin
```

- [ ] **Step 5: Extract and create the VNG IAM credentials**

```bash
AK=$(kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-deepseek-aiplatform-model \
       -o jsonpath='{.config.auth.iam_access_key}')
SK=$(kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-deepseek-aiplatform-model \
       -o jsonpath='{.config.auth.iam_secret_key}')
kubectl -n $NS create secret generic vng-iam-credentials \
  --from-literal=access_key="$AK" --from-literal=secret_key="$SK"
unset AK SK
```

- [ ] **Step 6: Verify all six secrets exist and none is empty**

```bash
for s in openai-byok-11374 gemini-api-key anthropic-api-key \
         deepseek-direct-key deepseek-dashscope-key vng-iam-credentials; do
  n=$(kubectl -n $NS get secret $s -o jsonpath='{.data}' | tr -d '{}' | wc -c)
  echo "$s bytes=$n"
done
```

Expected: six lines, every `bytes=` value greater than 10.

- [ ] **Step 7: Verify no credential was written to the repo**

```bash
cd $AGW_REPO && git status --porcelain pilot/
grep -rIl -e 'sk-' -e 'AIzaSy' pilot/ || echo "clean"
```

Expected: `clean`

---

### Task 6: First model end-to-end — BYOK OpenAI + chat HTTPRoute

This is the task that proves the whole routing approach. Everything after it is repetition of a working pattern.

**Files:**
- Create: `pilot/03-model-openai.yaml`
- Create: `pilot/02-httproutes.yaml`

- [ ] **Step 1: Write the failing verification**

```bash
mkdir -p $AGW_REPO/pilot/verify
```

Save as `pilot/verify/chat.sh`:

```bash
#!/usr/bin/env bash
# Usage: chat.sh <host> <legacy-path> <model>
set -euo pipefail
HOST="$1"; PATH_="$2"; MODEL="$3"
curl -sk -o /tmp/resp.json -w '%{http_code}' \
  -X POST "https://${HOST}${PATH_}" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"say OK\"}],\"max_tokens\":10}"
echo
jq -e '.choices[0].message.content' /tmp/resp.json
```

```bash
chmod +x $AGW_REPO/pilot/verify/chat.sh
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11374/openai/gpt-4o/v1/chat/completions gpt-4o
```

Expected: prints `404`, then `jq` exits non-zero. No route exists yet.

- [ ] **Step 3: Write the AgentgatewayModel**

`pilot/03-model-openai.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: gpt-4o
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: gpt-4o
  provider: OpenAI
  visibility: Public
  policies:
    auth:
      key:
        secretRef:
          name: openai-byok-11374
          key: key
```

- [ ] **Step 4: Write the chat-completions HTTPRoute**

The regex accepts the optional `/maas/user-N/` prefix and any `provider/model` segments, then rewrites to the canonical path. Model selection happens from `body.model`.

`pilot/02-httproutes.yaml`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: legacy-chat-completions
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - name: maas-v2-agw
  rules:
    - matches:
        - method: POST
          path:
            type: RegularExpression
            value: "^/(?:maas/user-[^/]+/)?[^/]+/[^/]+/v1/chat/completions$"
      filters:
        - type: URLRewrite
          urlRewrite:
            path:
              type: ReplaceFullPath
              replaceFullPath: /v1/chat/completions
```

- [ ] **Step 5: Apply both**

```bash
kubectl apply -f $AGW_REPO/pilot/03-model-openai.yaml
kubectl apply -f $AGW_REPO/pilot/02-httproutes.yaml
kubectl -n $NS wait --for=condition=Accepted httproute/legacy-chat-completions --timeout=60s
```

Expected: `condition met`

- [ ] **Step 6: Run the verification again**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11374/openai/gpt-4o/v1/chat/completions gpt-4o
```

Expected: prints `200`, then `jq` prints the assistant's content string.

- [ ] **Step 7: Verify streaming works**

```bash
curl -skN -X POST "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"count to 3"}],"stream":true,"max_tokens":20}' \
  | head -5
```

Expected: several `data: {...}` SSE lines.

- [ ] **Step 8: Commit**

```bash
cd $AGW_REPO && git add pilot/02-httproutes.yaml pilot/03-model-openai.yaml pilot/verify/chat.sh
git commit -m "pilot: first model end-to-end (BYOK OpenAI) via legacy chat path"
```

---

### Task 7: Gemini model and the generateContent surface

**Files:**
- Create: `pilot/04-model-gemini.yaml`
- Modify: `pilot/02-httproutes.yaml` (append a second HTTPRoute)

- [ ] **Step 1: Run the verification to confirm Gemini is not yet routable**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/gemini/gemini-2.5-flash/v1/chat/completions gemini-2.5-flash
```

Expected: non-200, or a body without `.choices[0].message.content`. The chat route matches, but no model named `gemini-2.5-flash` is registered.

- [ ] **Step 2: Write the Gemini model**

`pilot/04-model-gemini.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: gemini-2-5-flash
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: gemini-2.5-flash
  provider: Gemini
  visibility: Public
  policies:
    auth:
      key:
        secretRef:
          name: gemini-api-key
          key: key
```

- [ ] **Step 3: Append the generateContent HTTPRoute**

Append to `pilot/02-httproutes.yaml`:

```yaml
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: legacy-generate-content
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - name: maas-v2-agw
  rules:
    - matches:
        - method: POST
          path:
            type: RegularExpression
            value: "^/(?:maas/user-[^/]+/)?[^/]+/[^/]+/v1/models/[^/]+:(?:streamG|g)enerateContent$"
```

No rewrite: agentgateway's `extract_model_from_path` understands the `:generateContent` form natively, so the path is left intact and the model is taken from it.

- [ ] **Step 4: Apply and wait**

```bash
kubectl apply -f $AGW_REPO/pilot/04-model-gemini.yaml
kubectl apply -f $AGW_REPO/pilot/02-httproutes.yaml
kubectl -n $NS wait --for=condition=Accepted httproute/legacy-generate-content --timeout=60s
```

Expected: `condition met`

- [ ] **Step 5: Verify the chat surface**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/gemini/gemini-2.5-flash/v1/chat/completions gemini-2.5-flash
```

Expected: `200` and a content string.

- [ ] **Step 6: Verify the generateContent surface**

```bash
curl -sk -o /tmp/g.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11377/gemini/gemini-2.5-flash/v1/models/gemini-2.5-flash:generateContent" \
  -H 'Content-Type: application/json' \
  -d '{"contents":[{"parts":[{"text":"say OK"}]}]}'
jq -e '.candidates[0].content.parts[0].text' /tmp/g.json
```

Expected: `200`, then the text.

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/02-httproutes.yaml pilot/04-model-gemini.yaml
git commit -m "pilot: add Gemini model and generateContent route"
```

---

### Task 8: Anthropic model and the messages surface

**Files:**
- Create: `pilot/05-model-claude.yaml`
- Modify: `pilot/02-httproutes.yaml` (append a third HTTPRoute)

- [ ] **Step 1: Confirm Claude is not yet routable on the chat surface**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/anthropic/claude-sonnet-4-0/v1/chat/completions claude-sonnet-4-0
```

Expected: non-200 or missing content.

- [ ] **Step 2: Write the Anthropic model**

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: claude-sonnet-4-0
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: claude-sonnet-4-0
  provider: Anthropic
  visibility: Public
  policies:
    auth:
      key:
        secretRef:
          name: anthropic-api-key
          key: key
```

- [ ] **Step 3: Append the messages HTTPRoute**

Append to `pilot/02-httproutes.yaml`:

```yaml
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: legacy-messages
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - name: maas-v2-agw
  rules:
    - matches:
        - method: POST
          path:
            type: RegularExpression
            value: "^/(?:maas/user-[^/]+/)?[^/]+/[^/]+/v1/messages$"
      filters:
        - type: URLRewrite
          urlRewrite:
            path:
              type: ReplaceFullPath
              replaceFullPath: /v1/messages
```

- [ ] **Step 4: Apply and wait**

```bash
kubectl apply -f $AGW_REPO/pilot/05-model-claude.yaml
kubectl apply -f $AGW_REPO/pilot/02-httproutes.yaml
kubectl -n $NS wait --for=condition=Accepted httproute/legacy-messages --timeout=60s
```

Expected: `condition met`

- [ ] **Step 5: Verify the chat surface (OpenAI format in, Anthropic upstream)**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/anthropic/claude-sonnet-4-0/v1/chat/completions claude-sonnet-4-0
```

Expected: `200` and a content string. This proves OpenAI→Anthropic conversion.

- [ ] **Step 6: Verify the native messages surface**

```bash
curl -sk -o /tmp/m.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11377/anthropic/claude-sonnet-4-0/v1/messages" \
  -H 'Content-Type: application/json' \
  -d '{"model":"claude-sonnet-4-0","max_tokens":10,"messages":[{"role":"user","content":"say OK"}]}'
jq -e '.content[0].text' /tmp/m.json
```

Expected: `200`, then the text.

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/02-httproutes.yaml pilot/05-model-claude.yaml
git commit -m "pilot: add Anthropic model and messages route"
```

---

### Task 9: DeepSeek weighted virtual model

Replaces Kong's `balancer.targets[]` 99:1 split with two `Internal` models plus one `Public` virtual model.

**Files:**
- Create: `pilot/06-model-deepseek.yaml`

- [ ] **Step 1: Write all three resources**

`pilot/06-model-deepseek.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: deepseek-v4-pro-direct
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: deepseek-v4-pro-direct
  provider: OpenAI
  baseURL: https://api.deepseek.com
  visibility: Internal
  policies:
    auth:
      key:
        secretRef:
          name: deepseek-direct-key
          key: key
---
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: deepseek-v4-pro-dashscope
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: deepseek-v4-pro-dashscope
  provider: OpenAI
  baseURL: https://dashscope-intl.aliyuncs.com/compatible-mode/v1
  visibility: Internal
  policies:
    auth:
      key:
        secretRef:
          name: deepseek-dashscope-key
          key: key
---
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: deepseek-v4-pro
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: deepseek-v4-pro
  visibility: Public
  virtualModel:
    weighted:
      targets:
        - modelRef:
            name: deepseek-v4-pro-direct
          weight: 99
        - modelRef:
            name: deepseek-v4-pro-dashscope
          weight: 1
```

- [ ] **Step 2: Apply**

```bash
kubectl apply -f $AGW_REPO/pilot/06-model-deepseek.yaml
kubectl -n $NS get agentgatewaymodel
```

Expected: all three listed alongside the earlier models.

- [ ] **Step 3: Verify the virtual model serves traffic**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/deepseek/deepseek-v4-pro/v1/chat/completions deepseek-v4-pro
```

Expected: `200` and a content string.

- [ ] **Step 4: Verify Internal models are NOT directly addressable**

```bash
curl -sk -o /tmp/i.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11377/deepseek/deepseek-v4-pro-direct/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4-pro-direct","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'
```

Expected: `404` (or another non-2xx). A `200` here means `visibility: Internal` is not being enforced — stop and investigate before continuing.

- [ ] **Step 5: Verify the split ratio over 100 requests**

```bash
for i in $(seq 1 100); do
  $AGW_REPO/pilot/verify/chat.sh $GW_HOST \
    /maas/user-11377/deepseek/deepseek-v4-pro/v1/chat/completions deepseek-v4-pro >/dev/null 2>&1
done
kubectl -n $NS exec deploy/maas-v2-agw -c agentgateway -- \
  curl -s localhost:15020/metrics | grep -E 'agentgateway_llm.*deepseek'
```

Expected: counters for both backends, direct ≫ dashscope. With weights 99:1 across 100 requests, roughly 0–4 requests to dashscope is normal.

- [ ] **Step 6: Commit**

```bash
cd $AGW_REPO && git add pilot/06-model-deepseek.yaml
git commit -m "pilot: add DeepSeek 99:1 weighted virtual model"
```

---

### Task 10: VNG IAM token refresher

The aiplatform vLLM backend needs a bearer token obtained by exchanging IAM credentials. Tokens expire, so a CronJob refreshes a Secret the model references.

**Files:**
- Create: `pilot/iam-refresher/main.go`
- Create: `pilot/iam-refresher/main_test.go`
- Create: `pilot/iam-refresher/Dockerfile`
- Create: `pilot/12-iam-refresher.yaml`

- [ ] **Step 1: Write the failing test**

`pilot/iam-refresher/main_test.go`:

```go
package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestExchangeReturnsAccessToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			t.Errorf("expected POST, got %s", r.Method)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{"access_token": "tok-123", "expires_in": 3600})
	}))
	defer srv.Close()

	got, err := exchange(srv.URL, "ak", "sk")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "tok-123" {
		t.Fatalf("got %q, want %q", got, "tok-123")
	}
}

func TestExchangeErrorsOnNon200(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer srv.Close()

	if _, err := exchange(srv.URL, "ak", "sk"); err == nil {
		t.Fatal("expected an error for 401, got nil")
	}
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd $AGW_REPO/pilot/iam-refresher && go mod init iam-refresher 2>/dev/null; go test ./...
```

Expected: FAIL — `undefined: exchange`

- [ ] **Step 3: Write the implementation**

`pilot/iam-refresher/main.go`:

```go
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"
)

// exchange trades IAM client credentials for a bearer token.
func exchange(authURL, accessKey, secretKey string) (string, error) {
	body, err := json.Marshal(map[string]string{
		"clientId":     accessKey,
		"clientSecret": secretKey,
	})
	if err != nil {
		return "", err
	}
	req, err := http.NewRequest(http.MethodPost, authURL, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("iam auth returned %d", resp.StatusCode)
	}
	var out struct {
		AccessToken string `json:"access_token"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return "", err
	}
	if out.AccessToken == "" {
		return "", fmt.Errorf("iam auth returned an empty access_token")
	}
	return out.AccessToken, nil
}

func main() {
	authURL := os.Getenv("IAM_AUTH_URL")
	ns := os.Getenv("TARGET_NAMESPACE")
	secretName := os.Getenv("TARGET_SECRET")

	token, err := exchange(authURL, os.Getenv("IAM_ACCESS_KEY"), os.Getenv("IAM_SECRET_KEY"))
	if err != nil {
		log.Fatalf("token exchange failed: %v", err)
	}

	cfg, err := rest.InClusterConfig()
	if err != nil {
		log.Fatalf("in-cluster config: %v", err)
	}
	cs, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		log.Fatalf("clientset: %v", err)
	}

	ctx := context.Background()
	sec, err := cs.CoreV1().Secrets(ns).Get(ctx, secretName, metav1.GetOptions{})
	if err != nil {
		log.Fatalf("get secret: %v", err)
	}
	if sec.Data == nil {
		sec.Data = map[string][]byte{}
	}
	sec.Data["key"] = []byte(token)
	if _, err := cs.CoreV1().Secrets(ns).Update(ctx, sec, metav1.UpdateOptions{}); err != nil {
		log.Fatalf("update secret: %v", err)
	}
	log.Printf("refreshed %s/%s", ns, secretName)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd $AGW_REPO/pilot/iam-refresher
go get k8s.io/client-go@latest k8s.io/apimachinery@latest
go mod tidy
go test ./...
```

Expected: `ok  	iam-refresher`

- [ ] **Step 5: Commit the refresher code**

```bash
cd $AGW_REPO && git add pilot/iam-refresher/
git commit -m "pilot: add VNG IAM token refresher with tests"
```

- [ ] **Step 6: Build and push the image**

```bash
cd $AGW_REPO/pilot/iam-refresher
cat > Dockerfile <<'EOF'
FROM golang:1.22 AS build
WORKDIR /src
COPY . .
RUN CGO_ENABLED=0 go build -o /iam-refresher .

FROM gcr.io/distroless/static-debian12
COPY --from=build /iam-refresher /iam-refresher
ENTRYPOINT ["/iam-refresher"]
EOF

IMG=vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/iam-refresher:pilot-1
docker build -t $IMG . && docker push $IMG
echo $IMG
```

- [ ] **Step 7: Create the target secret, RBAC, and CronJob**

```bash
kubectl -n $NS create secret generic vng-iam-token --from-literal=key=placeholder
```

`pilot/12-iam-refresher.yaml`:

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: iam-refresher
  namespace: user-11377-maas-v2-agw
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: iam-refresher
  namespace: user-11377-maas-v2-agw
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    resourceNames: ["vng-iam-token"]
    verbs: ["get", "update"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: iam-refresher
  namespace: user-11377-maas-v2-agw
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: iam-refresher
subjects:
  - kind: ServiceAccount
    name: iam-refresher
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: iam-refresher
  namespace: user-11377-maas-v2-agw
spec:
  schedule: "*/10 * * * *"
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      template:
        spec:
          serviceAccountName: iam-refresher
          restartPolicy: OnFailure
          containers:
            - name: refresher
              image: vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/iam-refresher:pilot-1
              env:
                - name: IAM_AUTH_URL
                  value: https://iamapis.vngcloud.vn/accounts-api/v2/auth/token
                - name: TARGET_NAMESPACE
                  value: user-11377-maas-v2-agw
                - name: TARGET_SECRET
                  value: vng-iam-token
                - name: IAM_ACCESS_KEY
                  valueFrom:
                    secretKeyRef: { name: vng-iam-credentials, key: access_key }
                - name: IAM_SECRET_KEY
                  valueFrom:
                    secretKeyRef: { name: vng-iam-credentials, key: secret_key }
```

- [ ] **Step 8: Apply and trigger one run immediately**

```bash
kubectl apply -f $AGW_REPO/pilot/12-iam-refresher.yaml
kubectl -n $NS create job --from=cronjob/iam-refresher iam-refresher-manual
kubectl -n $NS wait --for=condition=complete job/iam-refresher-manual --timeout=120s
```

Expected: `job.batch/iam-refresher-manual condition met`

- [ ] **Step 9: Verify the token was written and is not the placeholder**

```bash
kubectl -n $NS get secret vng-iam-token -o jsonpath='{.data.key}' | base64 -d | head -c 12; echo
```

Expected: the first characters of a real token, not `placeholder`.

- [ ] **Step 10: Commit**

```bash
cd $AGW_REPO && git add pilot/12-iam-refresher.yaml
git commit -m "pilot: add IAM token refresher CronJob and RBAC"
```

---

### Task 11: vLLM model on the Custom provider

**Files:**
- Create: `pilot/07-model-vllm.yaml`

- [ ] **Step 1: Resolve the aiplatform endpoint base URL**

```bash
kubectl -n $SRC_NS get kongplugin 11377-maas-no-delete-deepseek-aiplatform-model \
  -o jsonpath='{.config.model.options.aiplatform}{"\n"}'
```

Expected: `{"endpoint_id":"me-8e5e98cf-622b-41ae-9c99-fc6b8a7b67e1","endpoint_version":"v1"}`

Confirm the full upstream URL the Kong driver builds from these fields by reading
`$AGW_REPO/../kong/kong/llm/drivers/aiplatform_vllm.lua`, then use it as `baseURL` below.

- [ ] **Step 2: Write the model**

`pilot/07-model-vllm.yaml` — replace `<AIPLATFORM_BASE_URL>` with the URL confirmed in Step 1:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayModel
metadata:
  name: qwen2-0-5b
  namespace: user-11377-maas-v2-agw
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  match:
    model: qwen2-0.5b
  provider: Custom
  baseURL: <AIPLATFORM_BASE_URL>
  visibility: Public
  custom:
    format: completions
  policies:
    auth:
      key:
        secretRef:
          name: vng-iam-token
          key: key
```

- [ ] **Step 3: Apply and verify**

```bash
kubectl apply -f $AGW_REPO/pilot/07-model-vllm.yaml
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/aiplatform/qwen2-0.5b/v1/chat/completions qwen2-0.5b
```

Expected: `200` and a content string.

- [ ] **Step 4: Verify the model picks up a refreshed token without a restart**

```bash
kubectl -n $NS create job --from=cronjob/iam-refresher iam-refresher-manual-2
kubectl -n $NS wait --for=condition=complete job/iam-refresher-manual-2 --timeout=120s
sleep 15
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/aiplatform/qwen2-0.5b/v1/chat/completions qwen2-0.5b
```

Expected: `200`. If this fails while Step 3 succeeded, agentgateway is not hot-reloading the
Secret — record it and add a `kubectl rollout restart deploy/maas-v2-agw` step to the CronJob as a
workaround, then re-run.

- [ ] **Step 5: Commit**

```bash
cd $AGW_REPO && git add pilot/07-model-vllm.yaml
git commit -m "pilot: add aiplatform vLLM model via Custom provider and IAM token"
```

---

# Phase 3 — Governance

### Task 12: Build the ext_authz adapter

Translates agentgateway's status-code-based HTTP ext_authz into the Rust server's always-200 JSON contract.

**Files:**
- Create: `pilot/adapter/main.go`
- Create: `pilot/adapter/main_test.go`
- Create: `pilot/adapter/Dockerfile`

- [ ] **Step 1: Write the failing tests**

`pilot/adapter/main_test.go`:

```go
package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func upstream(t *testing.T, payload map[string]any, status int) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(status)
		json.NewEncoder(w).Encode(payload)
	}))
}

func TestAllowReturns200WithTenantHeader(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": true, "tenant": "acme"}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 200 {
		t.Fatalf("got status %d, want 200", rec.Code)
	}
	if got := rec.Header().Get("X-Tenant-ID"); got != "acme" {
		t.Fatalf("got tenant %q, want %q", got, "acme")
	}
}

func TestDenyMapsBodyStatusCodeToHTTPStatus(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": false, "status_code": 429, "reason": "quota"}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 429 {
		t.Fatalf("got status %d, want 429", rec.Code)
	}
}

func TestDenyWithoutStatusCodeDefaultsTo403(t *testing.T) {
	srv := upstream(t, map[string]any{"allowed": false}, 200)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 403 {
		t.Fatalf("got status %d, want 403", rec.Code)
	}
}

func TestUpstreamNon200FailsClosedWith503(t *testing.T) {
	srv := upstream(t, map[string]any{}, 500)
	defer srv.Close()

	rec := httptest.NewRecorder()
	newHandler(srv.URL).ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}

func TestUnreachableUpstreamFailsClosedWith503(t *testing.T) {
	rec := httptest.NewRecorder()
	newHandler("http://127.0.0.1:1").ServeHTTP(rec, httptest.NewRequest("POST", "/check", strings.NewReader("{}")))

	if rec.Code != 503 {
		t.Fatalf("got status %d, want 503", rec.Code)
	}
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd $AGW_REPO/pilot/adapter && go mod init extauthz-adapter 2>/dev/null; go test ./...
```

Expected: FAIL — `undefined: newHandler`

- [ ] **Step 3: Write the implementation**

`pilot/adapter/main.go`:

```go
package main

import (
	"bytes"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"os"
	"time"
)

type checkResponse struct {
	Allowed    bool              `json:"allowed"`
	StatusCode int               `json:"status_code"`
	Reason     string            `json:"reason"`
	Tenant     string            `json:"tenant"`
	RateLimits map[string]string `json:"rate_limits"`
}

// newHandler returns an ext_authz endpoint that proxies to the Rust server's
// always-200 /v1/check contract and converts the JSON decision into an HTTP
// status code, which is what agentgateway's HTTP ext_authz acts on.
func newHandler(serverURL string) http.Handler {
	client := &http.Client{Timeout: 3 * time.Second}

	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			http.Error(w, "cannot read body", http.StatusServiceUnavailable)
			return
		}

		req, err := http.NewRequest(http.MethodPost, serverURL+"/v1/check", bytes.NewReader(body))
		if err != nil {
			http.Error(w, "cannot build request", http.StatusServiceUnavailable)
			return
		}
		req.Header.Set("Content-Type", "application/json")
		for _, h := range []string{"Authorization", "X-Model", "X-Estimated-Tokens", "X-Request-Id"} {
			if v := r.Header.Get(h); v != "" {
				req.Header.Set(h, v)
			}
		}

		resp, err := client.Do(req)
		if err != nil {
			log.Printf("check transport error: %v", err)
			http.Error(w, "authorization unavailable", http.StatusServiceUnavailable)
			return
		}
		defer resp.Body.Close()

		// Any non-200 violates the always-200 contract: fail closed.
		if resp.StatusCode != http.StatusOK {
			log.Printf("check contract violation: upstream status %d", resp.StatusCode)
			http.Error(w, "authorization unavailable", http.StatusServiceUnavailable)
			return
		}

		var decision checkResponse
		if err := json.NewDecoder(resp.Body).Decode(&decision); err != nil {
			log.Printf("check decode error: %v", err)
			http.Error(w, "authorization unavailable", http.StatusServiceUnavailable)
			return
		}

		if !decision.Allowed {
			status := decision.StatusCode
			if status < 400 || status > 599 {
				status = http.StatusForbidden
			}
			w.Header().Set("X-Auth-Reason", decision.Reason)
			w.WriteHeader(status)
			return
		}

		if decision.Tenant != "" {
			w.Header().Set("X-Tenant-ID", decision.Tenant)
		}
		for k, v := range decision.RateLimits {
			w.Header().Set("X-RateLimit-"+k, v)
		}
		w.WriteHeader(http.StatusOK)
	})
}

func main() {
	serverURL := os.Getenv("RUST_SERVER_URL")
	if serverURL == "" {
		log.Fatal("RUST_SERVER_URL is required")
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	mux := http.NewServeMux()
	mux.Handle("/check", newHandler(serverURL))
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})

	log.Printf("extauthz-adapter listening on :%s -> %s", port, serverURL)
	srv := &http.Server{
		Addr:              ":" + port,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	if err := srv.ListenAndServe(); err != nil {
		log.Fatal(err)
	}
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd $AGW_REPO/pilot/adapter && go mod tidy && go vet ./... && go test ./... -v
```

Expected: all five tests `PASS`, then `ok  	extauthz-adapter`. No vet output.

- [ ] **Step 5: Commit**

```bash
cd $AGW_REPO && git add pilot/adapter/
git commit -m "pilot: add ext_authz adapter translating always-200 contract to status codes"
```

---

### Task 13: Deploy the pilot Rust server and the adapter

**Files:**
- Create: `pilot/adapter/Dockerfile`
- Create: `pilot/10-adapter.yaml`
- Create: `pilot/11-plugin-server.yaml`

- [ ] **Step 1: Build and push the adapter image**

```bash
cd $AGW_REPO/pilot/adapter
cat > Dockerfile <<'EOF'
FROM golang:1.22 AS build
WORKDIR /src
COPY . .
RUN CGO_ENABLED=0 go build -o /adapter .

FROM gcr.io/distroless/static-debian12
COPY --from=build /adapter /adapter
EXPOSE 8080
ENTRYPOINT ["/adapter"]
EOF

IMG=vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/extauthz-adapter:pilot-1
docker build -t $IMG . && docker push $IMG
```

- [ ] **Step 2: Copy the production plugin-server spec as a starting point**

```bash
kubectl -n $SRC_NS get deploy ai-gateway-plugin-server -o yaml \
  > /tmp/plugin-server-src.yaml
grep -E 'image:|env:|- name:' /tmp/plugin-server-src.yaml | head -40
```

Review its env vars — in particular any database DSN — so the pilot copy can be pointed at its
own database.

- [ ] **Step 2b: Provision the pilot database and seed one tenant**

The pilot MUST NOT share production's database (design §1.1) — without `/v1/usage` true-up it
would leave real tenant budgets permanently under-debited.

Determine from Step 2 which backing store the server uses, then create an isolated instance of it.
If it is PostgreSQL, the fastest route is a single-pod instance in the pilot namespace:

```bash
kubectl -n $NS create deployment pilot-db --image=postgres:16 \
  --port=5432 -- postgres
kubectl -n $NS set env deploy/pilot-db \
  POSTGRES_PASSWORD=pilot POSTGRES_USER=pilot POSTGRES_DB=aigw
kubectl -n $NS expose deploy/pilot-db --port=5432 --name=pilot-db
kubectl -n $NS rollout status deploy/pilot-db --timeout=120s
```

The resulting DSN for Step 3 is:
`postgres://pilot:pilot@pilot-db.user-11377-maas-v2-agw.svc.cluster.local:5432/aigw`

After the server is running (Step 5), seed one tenant with a known API key using whatever
mechanism the server provides — check its admin endpoints or migrations:

```bash
kubectl -n $NS exec deploy/ai-gateway-plugin-server -- /bin/sh -c 'ls /usr/local/bin; env | grep -i admin'
```

Export the seeded key for later tasks. Every subsequent authenticated call depends on it:

```bash
export PILOT_API_KEY=<the-seeded-key>
```

- [ ] **Step 3: Write the pilot plugin-server manifest**

`pilot/11-plugin-server.yaml` — set `<PILOT_DB_DSN>` to the DSN from Step 2b
(`postgres://pilot:pilot@pilot-db.user-11377-maas-v2-agw.svc.cluster.local:5432/aigw` if you used
the Postgres path). It must not be production's:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ai-gateway-plugin-server
  namespace: user-11377-maas-v2-agw
spec:
  replicas: 1
  selector:
    matchLabels: { app: ai-gateway-plugin-server }
  template:
    metadata:
      labels: { app: ai-gateway-plugin-server }
    spec:
      containers:
        - name: server
          image: vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/ai-gateway-plugin-server:maas-v2
          ports:
            - containerPort: 8080
          env:
            - name: DATABASE_URL
              value: "<PILOT_DB_DSN>"
---
apiVersion: v1
kind: Service
metadata:
  name: ai-gateway-plugin-server
  namespace: user-11377-maas-v2-agw
spec:
  selector: { app: ai-gateway-plugin-server }
  ports:
    - port: 8080
      targetPort: 8080
```

- [ ] **Step 4: Write the adapter manifest**

`pilot/10-adapter.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: extauthz-adapter
  namespace: user-11377-maas-v2-agw
spec:
  replicas: 2
  selector:
    matchLabels: { app: extauthz-adapter }
  template:
    metadata:
      labels: { app: extauthz-adapter }
    spec:
      containers:
        - name: adapter
          image: vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/extauthz-adapter:pilot-1
          ports:
            - containerPort: 8080
          env:
            - name: RUST_SERVER_URL
              value: http://ai-gateway-plugin-server.user-11377-maas-v2-agw.svc.cluster.local:8080
          readinessProbe:
            httpGet: { path: /healthz, port: 8080 }
---
apiVersion: v1
kind: Service
metadata:
  name: extauthz-adapter
  namespace: user-11377-maas-v2-agw
spec:
  selector: { app: extauthz-adapter }
  ports:
    - port: 8080
      targetPort: 8080
```

- [ ] **Step 5: Apply both and wait**

```bash
kubectl apply -f $AGW_REPO/pilot/11-plugin-server.yaml
kubectl apply -f $AGW_REPO/pilot/10-adapter.yaml
kubectl -n $NS rollout status deploy/ai-gateway-plugin-server --timeout=180s
kubectl -n $NS rollout status deploy/extauthz-adapter --timeout=120s
```

Expected: both `successfully rolled out`

- [ ] **Step 6: Verify the adapter reaches the Rust server in-cluster**

```bash
kubectl -n $NS run curl-test --rm -it --restart=Never --image=curlimages/curl -- \
  -s -o /dev/null -w '%{http_code}\n' \
  -X POST http://extauthz-adapter:8080/check -d '{}'
```

Expected: a status code that is **not** `000`. A `503` here is a valid result (the Rust server
denies an empty payload); it proves the chain is wired. A connection failure is not.

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/10-adapter.yaml pilot/11-plugin-server.yaml pilot/adapter/Dockerfile
git commit -m "pilot: deploy ext_authz adapter and isolated plugin-server copy"
```

---

### Task 14: Wire extAuth into the gateway

**Files:**
- Create: `pilot/08-policy-extauth.yaml`

- [ ] **Step 1: Confirm requests currently succeed without authorization**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11374/openai/gpt-4o/v1/chat/completions gpt-4o
```

Expected: `200`. No auth is enforced yet — this is the "before" state.

- [ ] **Step 2: Write the policy**

`pilot/08-policy-extauth.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayPolicy
metadata:
  name: maas-v2-agw-extauth
  namespace: user-11377-maas-v2-agw
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  traffic:
    extAuth:
      failureMode: deny
      protocol:
        http:
          path: '"/check"'
      includeRequestBody:
        maxBytes: 8192
      includeResponseHeaders:
        - X-Tenant-ID
      backendRef:
        name: extauthz-adapter
        port: 8080
```

- [ ] **Step 3: Apply and confirm the policy is accepted**

```bash
kubectl apply -f $AGW_REPO/pilot/08-policy-extauth.yaml
kubectl -n $NS get agentgatewaypolicy maas-v2-agw-extauth -o yaml | tail -20
```

Expected: a status block with an `Accepted: True` condition.

- [ ] **Step 4: Verify an unauthenticated request is now rejected**

```bash
curl -sk -o /dev/null -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'
```

Expected: `401` or `403` — not `200`. A `200` means extAuth is not being applied; stop and check
the policy's `targetRefs`.

- [ ] **Step 5: Verify an authenticated request succeeds**

Obtain a valid API key for the pilot Rust server's tenant database, then:

```bash
curl -sk -o /tmp/a.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $PILOT_API_KEY" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"say OK"}],"max_tokens":10}'
jq -e '.choices[0].message.content' /tmp/a.json
```

Expected: `200` and a content string.

- [ ] **Step 6: Verify fail-closed behavior**

```bash
kubectl -n $NS scale deploy/ai-gateway-plugin-server --replicas=0
sleep 10
curl -sk -o /dev/null -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H "Authorization: Bearer $PILOT_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'
kubectl -n $NS scale deploy/ai-gateway-plugin-server --replicas=1
kubectl -n $NS rollout status deploy/ai-gateway-plugin-server --timeout=120s
```

Expected: `503` while scaled to zero. This is the single most important safety check in the plan —
it proves the always-200 contract cannot leak through as an allow.

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/08-policy-extauth.yaml
git commit -m "pilot: enforce extAuth via adapter with fail-closed verification"
```

---

### Task 15: Add static guardrails

Kong resolves guardrail config per tenant at request time; agentgateway cannot, so the pilot
configures them statically per model. Applied to one model first to validate, then to the rest.

**Files:**
- Modify: `pilot/03-model-openai.yaml`

- [ ] **Step 1: Confirm a prompt that should be blocked currently succeeds**

```bash
curl -sk -o /tmp/g1.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H "Authorization: Bearer $PILOT_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"my SSN is 123-45-6789"}],"max_tokens":10}'
```

Expected: `200` — no guardrail is active yet.

- [ ] **Step 2: Add guardrails to the OpenAI model**

Add this `promptGuard` block under `spec.policies` in `pilot/03-model-openai.yaml`:

```yaml
    promptGuard:
      streaming: Enabled
      request:
        - openAIModeration:
            authorization:
              secretRef:
                name: openai-byok-11374
                key: key
        - regex:
            action: Reject
            rules:
              - pattern: '\b\d{3}-\d{2}-\d{4}\b'
                name: us-ssn
      response:
        - regex:
            action: Reject
            rules:
              - pattern: '\b\d{3}-\d{2}-\d{4}\b'
                name: us-ssn
```

- [ ] **Step 3: Apply**

```bash
kubectl apply -f $AGW_REPO/pilot/03-model-openai.yaml
sleep 10
```

- [ ] **Step 4: Verify the request guardrail rejects**

```bash
curl -sk -o /tmp/g2.json -w '%{http_code}\n' -X POST \
  "https://$GW_HOST/maas/user-11374/openai/gpt-4o/v1/chat/completions" \
  -H "Authorization: Bearer $PILOT_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"my SSN is 123-45-6789"}],"max_tokens":10}'
```

Expected: a 4xx rejection, not `200`.

- [ ] **Step 5: Verify a benign request still succeeds**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11374/openai/gpt-4o/v1/chat/completions gpt-4o
```

Expected: `200` and a content string. (Add the `Authorization` header to `chat.sh` first if you
have not already.)

- [ ] **Step 6: Replicate the same `promptGuard` block to the other four public models**

Apply the identical block to `pilot/04-model-gemini.yaml`, `pilot/05-model-claude.yaml`,
`pilot/06-model-deepseek.yaml` (on the `deepseek-v4-pro` virtual model only), and
`pilot/07-model-vllm.yaml`, then:

```bash
kubectl apply -f $AGW_REPO/pilot/
```

- [ ] **Step 7: Verify the SSN prompt is rejected on every public model**

```bash
for m in gpt-4o gemini-2.5-flash claude-sonnet-4-0 deepseek-v4-pro qwen2-0.5b; do
  code=$(curl -sk -o /dev/null -w '%{http_code}' -X POST \
    "https://$GW_HOST/maas/user-11377/x/$m/v1/chat/completions" \
    -H "Authorization: Bearer $PILOT_API_KEY" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$m\",\"messages\":[{\"role\":\"user\",\"content\":\"my SSN is 123-45-6789\"}],\"max_tokens\":5}")
  echo "$m -> $code"
done
```

Expected: five lines, every code a 4xx.

- [ ] **Step 8: Commit**

```bash
cd $AGW_REPO && git add pilot/0*-model-*.yaml
git commit -m "pilot: add static request/response/streaming guardrails to all public models"
```

---

# Phase 4 — Resilience and Observability

### Task 16: Health, eviction, and retry policies

**Files:**
- Modify: `pilot/06-model-deepseek.yaml`
- Create: `pilot/13-policy-resilience.yaml`

- [ ] **Step 1: Add a health policy to both DeepSeek internal models**

Add under `spec.policies` in each of `deepseek-v4-pro-direct` and `deepseek-v4-pro-dashscope`:

```yaml
    health:
      unhealthyExpression: 'response.code == 429 || response.code >= 500'
      eviction:
        duration: 30s
        consecutiveFailures: 2
```

- [ ] **Step 2: Write the retry policy**

`pilot/13-policy-resilience.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayPolicy
metadata:
  name: maas-v2-agw-resilience
  namespace: user-11377-maas-v2-agw
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  traffic:
    # NOTE: agentgateway computes attempts+1 despite the field docs saying
    # "total including original". attempts: 1 produces TWO upstream calls.
    retry:
      attempts: 1
      backoff: 200ms
      codes: [429, 500, 502, 503, 504]
    timeouts:
      request: 600s
```

- [ ] **Step 3: Apply**

```bash
kubectl apply -f $AGW_REPO/pilot/06-model-deepseek.yaml
kubectl apply -f $AGW_REPO/pilot/13-policy-resilience.yaml
```

- [ ] **Step 4: Force a failure and verify retry plus eviction**

Point the dashscope internal model at an unreachable base URL to guarantee failures:

```bash
kubectl -n $NS patch agentgatewaymodel deepseek-v4-pro-dashscope --type=merge \
  -p '{"spec":{"baseURL":"https://127.0.0.1:9"}}'

for i in $(seq 1 30); do
  $AGW_REPO/pilot/verify/chat.sh $GW_HOST \
    /maas/user-11377/deepseek/deepseek-v4-pro/v1/chat/completions deepseek-v4-pro >/dev/null 2>&1
done
```

- [ ] **Step 5: Verify requests still succeed despite the broken backend**

```bash
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11377/deepseek/deepseek-v4-pro/v1/chat/completions deepseek-v4-pro
```

Expected: `200`. The 1% of traffic hitting the broken backend is retried onto the healthy one, and
the broken backend is evicted after 2 consecutive failures.

- [ ] **Step 6: Restore the correct base URL**

```bash
kubectl apply -f $AGW_REPO/pilot/06-model-deepseek.yaml
```

- [ ] **Step 7: Commit**

```bash
cd $AGW_REPO && git add pilot/06-model-deepseek.yaml pilot/13-policy-resilience.yaml
git commit -m "pilot: add health eviction and retry policies"
```

---

### Task 17: Metrics, tracing, and access logs

**Files:**
- Create: `pilot/09-policy-observability.yaml`

- [ ] **Step 1: Confirm metrics are not yet scraped**

```bash
kubectl -n monitoring get servicemonitor,podmonitor | grep -i agw || echo "none yet"
```

Expected: `none yet`

- [ ] **Step 2: Write the observability policy**

`pilot/09-policy-observability.yaml`:

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayPolicy
metadata:
  name: maas-v2-agw-observability
  namespace: user-11377-maas-v2-agw
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: maas-v2-agw
  frontend:
    metrics:
      enabled: true
    tracing:
      enabled: true
    accessLog:
      enabled: true
```

- [ ] **Step 3: Apply and verify the metrics endpoint serves LLM metrics**

```bash
kubectl apply -f $AGW_REPO/pilot/09-policy-observability.yaml
$AGW_REPO/pilot/verify/chat.sh $GW_HOST \
  /maas/user-11374/openai/gpt-4o/v1/chat/completions gpt-4o >/dev/null

kubectl -n $NS exec deploy/maas-v2-agw -c agentgateway -- \
  curl -s localhost:15020/metrics | grep -c '^agentgateway_'
```

Expected: a count greater than 0.

- [ ] **Step 4: Verify token metrics carry model labels**

```bash
kubectl -n $NS exec deploy/maas-v2-agw -c agentgateway -- \
  curl -s localhost:15020/metrics | grep 'token' | head -5
```

Expected: metric lines including a `model` or `request_model` label.

- [ ] **Step 5: Create a PodMonitor so Prometheus scrapes the pilot**

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: maas-v2-agw
  namespace: user-11377-maas-v2-agw
  labels:
    release: monitoring
spec:
  selector:
    matchLabels:
      gateway.networking.k8s.io/gateway-name: maas-v2-agw
  podMetricsEndpoints:
    - port: metrics
      path: /metrics
```

Append this to `pilot/09-policy-observability.yaml`, separated by `---`, then apply:

```bash
kubectl apply -f $AGW_REPO/pilot/09-policy-observability.yaml
```

- [ ] **Step 6: Verify Prometheus picked up the target**

```bash
kubectl -n monitoring get podmonitor maas-v2-agw -o jsonpath='{.metadata.name}{"\n"}' 2>/dev/null \
  || kubectl -n $NS get podmonitor maas-v2-agw -o jsonpath='{.metadata.name}{"\n"}'
```

Expected: `maas-v2-agw`

Then confirm in the Prometheus UI that a target with job `user-11377-maas-v2-agw/maas-v2-agw` is
`UP`. If the `release: monitoring` label does not match your Prometheus selector, adjust it to
match the existing ServiceMonitors in the `monitoring` namespace.

- [ ] **Step 7: Verify access logs are emitted**

```bash
kubectl -n $NS logs deploy/maas-v2-agw -c agentgateway --tail=20 | grep -i 'chat/completions'
```

Expected: at least one log line for a proxied request.

- [ ] **Step 8: Commit**

```bash
cd $AGW_REPO && git add pilot/09-policy-observability.yaml
git commit -m "pilot: enable metrics, tracing, access logs and Prometheus scraping"
```

---

### Task 18: Final acceptance sweep

**Files:**
- Create: `pilot/verify/acceptance.sh`

- [ ] **Step 1: Write the acceptance script**

`pilot/verify/acceptance.sh`:

```bash
#!/usr/bin/env bash
# Runs every acceptance criterion from the design spec §10.
set -uo pipefail
HOST="${GW_HOST:?set GW_HOST}"
KEY="${PILOT_API_KEY:?set PILOT_API_KEY}"
pass=0; fail=0

check() { # check <name> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "PASS  $1"; pass=$((pass+1));
  else echo "FAIL  $1 (expected $2, got $3)"; fail=$((fail+1)); fi
}

call() { # call <model> <path> [content]
  curl -sk -o /tmp/acc.json -w '%{http_code}' -X POST "https://${HOST}$2" \
    -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"${3:-say OK}\"}],\"max_tokens\":10}"
}

# 1: all five models respond on legacy paths
for m in gpt-4o gemini-2.5-flash claude-sonnet-4-0 deepseek-v4-pro qwen2-0.5b; do
  check "model $m responds" 200 "$(call $m /maas/user-11377/x/$m/v1/chat/completions)"
done

# 2: streaming
n=$(curl -skN -X POST "https://${HOST}/maas/user-11377/x/gpt-4o/v1/chat/completions" \
     -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
     -d '{"model":"gpt-4o","messages":[{"role":"user","content":"count to 3"}],"stream":true,"max_tokens":20}' \
     | grep -c '^data:')
check "streaming emits SSE frames" "yes" "$([ "$n" -gt 0 ] && echo yes || echo no)"

# 3: unauthorized rejected
u=$(curl -sk -o /dev/null -w '%{http_code}' -X POST \
     "https://${HOST}/maas/user-11377/x/gpt-4o/v1/chat/completions" \
     -H 'Content-Type: application/json' \
     -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":5}')
check "unauthorized rejected" "yes" "$([ "$u" != "200" ] && echo yes || echo no)"

# 5: guardrail rejects
g=$(call gpt-4o /maas/user-11377/x/gpt-4o/v1/chat/completions "my SSN is 123-45-6789")
check "guardrail rejects SSN" "yes" "$([ "$g" != "200" ] && echo yes || echo no)"

echo "----"
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
```

```bash
chmod +x $AGW_REPO/pilot/verify/acceptance.sh
```

- [ ] **Step 2: Run it**

```bash
$AGW_REPO/pilot/verify/acceptance.sh
```

Expected: every line `PASS`, ending `passed=8 failed=0`.

- [ ] **Step 3: Verify criterion 4 (weighted split) from metrics**

```bash
kubectl -n $NS exec deploy/maas-v2-agw -c agentgateway -- \
  curl -s localhost:15020/metrics | grep -E 'deepseek-v4-pro-(direct|dashscope)'
```

Expected: both counters present, direct ≫ dashscope.

- [ ] **Step 4: Verify criterion 8 — no plaintext credentials in the repo**

```bash
cd $AGW_REPO
grep -rIn -e 'sk-ant' -e 'sk-[a-zA-Z0-9]\{20,\}' -e 'AIzaSy' pilot/ && echo "FOUND SECRETS" || echo "clean"
```

Expected: `clean`

- [ ] **Step 5: Confirm the Kong namespace was never modified**

```bash
kubectl -n $SRC_NS get gateway,httproute,kongplugin --no-headers | wc -l
```

Expected: `284` (1 gateway + 128 httproutes + 155 kongplugins), unchanged from the pre-pilot count.

- [ ] **Step 6: Commit**

```bash
cd $AGW_REPO && git add pilot/verify/acceptance.sh
git commit -m "pilot: add acceptance sweep covering design spec criteria"
```

---

## Deferred to Later Plans

| Item | Where |
|---|---|
| Kong-vs-agentgateway traffic replay and comparison | Phase 5 plan |
| `/v1/usage` TPM true-up (ext_proc or code change) | Phase 6 plan, design §6 / §9.1 |
| Per-tenant guardrail config injection | Phase 6 plan, design §9.1 |
| Web search / server-tool loop | Not planned; design §9 item 4 |
| Remaining 140 models, embeddings/images/rerank surfaces | Full-migration plan |
