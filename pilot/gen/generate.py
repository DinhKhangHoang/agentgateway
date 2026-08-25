#!/usr/bin/env python3
"""Kong MaaS v2 -> agentgateway parity generator.

Reads a live Kong namespace (KongPlugin + HTTPRoute) and emits agentgateway
CRs reproducing both layers of the Kong surface:

  Layer 1 (canonical)  -> AgentgatewayModel, body-`model`-routed on the
                          hardcoded canonical path list.
  Layer 2 (legacy URL) -> AgentgatewayBackend + HTTPRoute(backendRefs), the
                          only mechanism that works on arbitrary paths.

It is a translator, not a one-off: namespace, tenant and model list all come
from the cluster. Standard library only; `kubectl` is shelled out to for reads.
Reads only -- nothing is ever applied, patched or deleted.

Everything that cannot be reproduced faithfully is REFUSED and recorded in
out/report.md. Nothing is invented.
"""

import argparse
import collections
import json
import os
import re
import subprocess
import sys

# --------------------------------------------------------------------------
# Kong -> agentgateway constants
# --------------------------------------------------------------------------

# Kong route_type -> the canonical agentgateway path the legacy URL is
# rewritten to, so the AI backend's resolve_route() picks the right RouteType.
ROUTE_TYPE_PATH = {
    "llm/v1/chat": "/v1/chat/completions",
    "llm/v1/completions": "/v1/completions",
    "llm/v1/embeddings": "/v1/embeddings",
    "llm/v1/messages": "/v1/messages",
    "llm/v1/responses": "/v1/responses",
    "llm/v1/rerank": "/v1/rerank",
    "llm/v1/images/generations": "/v1/images/generations",
    "llm/v1/generateContent": "/v1/generateContent",
}

# Paths outside the built-in model_router_matches() list, which therefore need
# an explicit spec.match.paths entry to route at all on the canonical surface.
NON_BUILTIN_PATHS = {"/v1/completions", "/v1/generateContent"}

# Endpoint suffixes stripped from Kong's fully-qualified `upstream_url` to
# recover a base URL. Kong points several route types at the same upstream
# endpoint (e.g. llm/v1/messages -> .../chat/completions) and relies on its own
# format translation, exactly as agentgateway does, so every known suffix is
# tried rather than only the one belonging to the row's route_type.
# Longest first: /chat/completions must beat /completions.
UPSTREAM_SUFFIXES = [
    "/chat/completions",
    "/images/generations",
    "/embeddings",
    "/completions",
    "/messages",
    "/responses",
    "/rerank",
]

# Kong model.provider -> AgentgatewayModel spec.provider enum value.
MODEL_PROVIDER = {
    "openai": "OpenAI",
    "gemini": "Gemini",
    "anthropic": "Anthropic",
    "deepseek": "Deepseek",
    "huggingface": "Huggingface",
}

# Kong model.provider -> AgentgatewayBackend provider type key. The backend CRD
# has no `deepseek`/`huggingface` key, so those OpenAI-dialect providers are
# expressed as `openai` plus an explicit host override.
BACKEND_PROVIDER = {
    "openai": "openai",
    "gemini": "gemini",
    "anthropic": "anthropic",
    "deepseek": "openai",
    "huggingface": "openai",
}

# Base URL a Kong provider implies when the plugin sets no `upstream_url`.
# Only needed where the agentgateway backend provider key differs from the Kong
# provider (see BACKEND_PROVIDER) and the default would therefore be wrong.
# Recorded in the report as a generator-supplied default.
PROVIDER_DEFAULT_BASE = {
    "deepseek": "https://api.deepseek.com/v1",
}

# Kong ai-proxy config that has no agentgateway equivalent and is dropped.
# feature key -> (bucket, human description)
UNSUPPORTED_FEATURES = {
    "web_search": "Kong-hosted web-search tool injection",
    "server_tools": "Kong-hosted server-side tool sidecar",
    "capacity": "Kong capacity_store admission control",
    "sticky": "Kong sticky target pinning",
}

LOSSY_OPTIONS = {
    "gemini_format": "upstream Gemini dialect selector; agentgateway's Gemini provider picks its own",
    "responses_upstream_format": "Kong-specific /v1/responses upstream dialect selector",
    "anthropic_version": "anthropic-version header pin; agentgateway's Anthropic provider sets its own",
    "glm_reasoning": "Kong-specific GLM reasoning-effort mapping",
    "aiplatform": "VNG AI-Platform endpoint indirection",
}

MAX_RULES_PER_HTTPROUTE = 16  # gateway.networking.k8s.io HTTPRoute spec.rules maxItems

GENERIC_PREFIX = "/(?:maas/user-[^/]+/)?"
TENANT_PREFIX_RE = re.compile(r"^/\(\?:maas/\)\?user-([0-9]+)/(.*)$")


# --------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------

def kubectl_json(kubeconfig, namespace, kind):
    """Read-only `kubectl get`. Never mutates anything."""
    cmd = ["kubectl", "--kubeconfig", kubeconfig, "-n", namespace,
           "get", kind, "-o", "json"]
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return json.loads(out)["items"]


def yq(s):
    """YAML single-quoted scalar. Safe for backslashes, colons, leading '*'."""
    return "'" + str(s).replace("'", "''") + "'"


def slug(s):
    s = re.sub(r"[^a-zA-Z0-9]+", "-", str(s)).strip("-").lower()
    return re.sub(r"-{2,}", "-", s) or "x"


def unescape_regex_literal(seg):
    """Turn a literal regex path segment back into the plain string.

    Returns None when the segment is not a literal (i.e. it is a wildcard such
    as `[^/]+`, so the client -- not the config -- chooses the model).
    """
    if re.search(r"[\[\](){}*+?|]", seg.replace(r"\.", "")):
        return None
    return seg.replace("\\.", ".").replace("\\-", "-")


def split_path_segments(rest):
    """Split a regex path on '/', treating a `[...]` character class as opaque."""
    out, cur, i = [], "", 0
    while i < len(rest):
        c = rest[i]
        if c == "[":
            j = rest.find("]", i)
            if j == -1:
                cur += rest[i:]
                break
            cur += rest[i:j + 1]
            i = j + 1
            continue
        if c == "/":
            out.append(cur)
            cur = ""
            i += 1
            continue
        cur += c
        i += 1
    out.append(cur)
    return out


def split_base_url(base):
    """base URL -> (scheme, host, port, path). Stdlib urlparse avoided only to
    keep the port defaulting explicit, which the CRD's CEL rule requires."""
    m = re.match(r"^(https?)://([^/:]+)(?::([0-9]+))?(/.*)?$", base)
    if not m:
        return None
    scheme, host, port, path = m.group(1), m.group(2), m.group(3), m.group(4) or "/"
    port = int(port) if port else (443 if scheme == "https" else 80)
    return scheme, host, port, path.rstrip("/") or "/"


def derive_base(upstream_url):
    """(base_url, is_canonical). is_canonical is False when the upstream path is
    not one agentgateway can derive from a route type, in which case the caller
    must use a full `path` override and cannot express the upstream as a model."""
    for suf in UPSTREAM_SUFFIXES:
        if upstream_url.endswith(suf):
            return upstream_url[: -len(suf)], True
    return upstream_url, False


# --------------------------------------------------------------------------
# extraction
# --------------------------------------------------------------------------

class Row(object):
    """One joined (HTTPRoute, ai-proxy KongPlugin) pair."""

    def __init__(self, route, plugin, cfg, path_value):
        self.route = route
        self.plugin = plugin
        self.cfg = cfg
        self.path_value = path_value
        self.tenant = None
        self.vendor = None
        self.model_seg = None
        self.verb = None
        self.parsed = False

        if path_value.startswith(GENERIC_PREFIX):
            rest = path_value[len(GENERIC_PREFIX):]
        else:
            m = TENANT_PREFIX_RE.match(path_value)
            if not m:
                return
            self.tenant = m.group(1)
            rest = m.group(2)
        segs = split_path_segments(rest.rstrip("$"))
        if len(segs) < 3:
            return
        self.vendor = segs[0]
        self.model_seg = segs[1]
        self.verb = "/" + "/".join(segs[2:])
        self.parsed = True

    # --- config accessors -------------------------------------------------
    @property
    def model_cfg(self):
        return self.cfg.get("model") or {}

    @property
    def options(self):
        return self.model_cfg.get("options") or {}

    @property
    def auth(self):
        return self.cfg.get("auth") or {}

    @property
    def route_type(self):
        return self.cfg.get("route_type")

    @property
    def kong_provider(self):
        return self.model_cfg.get("provider")

    @property
    def kong_model_name(self):
        return self.model_cfg.get("name")

    @property
    def upstream_url(self):
        return self.options.get("upstream_url")

    @property
    def model_literal(self):
        return unescape_regex_literal(self.model_seg) if self.model_seg else None

    @property
    def unsupported_features(self):
        found = []
        for key, desc in UNSUPPORTED_FEATURES.items():
            if self.model_cfg.get(key) or self.options.get(key) or self.cfg.get(key):
                found.append((key, desc))
        return found

    @property
    def lossy_options(self):
        return [(k, v) for k, v in sorted(LOSSY_OPTIONS.items()) if k in self.options]


def extract(kubeconfig, namespace):
    plugins = kubectl_json(kubeconfig, namespace, "kongplugin")
    routes = kubectl_json(kubeconfig, namespace, "httproute")
    ai_proxy = {p["metadata"]["name"]: p for p in plugins
                if p.get("plugin") == "ai-proxy"}

    rows, attached, routes_without_ai_proxy, unparsed = [], set(), [], []
    for r in sorted(routes, key=lambda x: x["metadata"]["name"]):
        rname = r["metadata"]["name"]
        ann = (r["metadata"].get("annotations") or {}).get("konghq.com/plugins", "")
        names = [x.strip() for x in ann.split(",") if x.strip()]
        mine = [n for n in names if n in ai_proxy]
        if not mine:
            routes_without_ai_proxy.append(rname)
            continue
        matches = []
        for rule in r["spec"].get("rules") or []:
            for m in rule.get("matches") or []:
                if "path" in m:
                    matches.append(m["path"])
        for pname in mine:
            attached.add(pname)
            for path in matches:
                row = Row(rname, pname, ai_proxy[pname].get("config") or {},
                          path.get("value", ""))
                if not row.parsed:
                    unparsed.append((rname, pname, path.get("value", "")))
                    continue
                rows.append(row)
    unreachable = sorted(set(ai_proxy) - attached)
    return ai_proxy, rows, unreachable, routes_without_ai_proxy, unparsed


# --------------------------------------------------------------------------
# classification
# --------------------------------------------------------------------------

class Report(object):
    def __init__(self):
        self.unsupported = []            # (route, plugin, what)
        self.refused_identity = []       # (route|'-', plugin, why)
        self.unreachable = []            # (plugin, why)
        self.inferred = []               # (route, plugin, name)
        self.losses = collections.OrderedDict()
        self.feature_scan = collections.OrderedDict(
            (k, 0) for k in sorted(UNSUPPORTED_FEATURES))
        self.counts = collections.OrderedDict()
        self.no_guardrail_models = []    # Detect-mapped models (see gen/README.md)

    def loss(self, heading, item):
        self.losses.setdefault(heading, []).append(item)


def classify(rows, unreachable, ai_proxy, rep):
    """Split rows into emittable / refused, and record every refusal."""
    keep = []
    for row in sorted(rows, key=lambda r: (r.route, r.plugin)):
        tag = "%s / %s" % (row.route, row.plugin)

        # --- hard refusals ------------------------------------------------
        if row.model_literal is None and not row.kong_model_name:
            rep.refused_identity.append(
                (row.route, row.plugin,
                 "route path pins no model (`%s`) and config.model.name is unset "
                 "-- the client chooses the model, so there is nothing to key an "
                 "AgentgatewayModel or a per-model backend on" % row.model_seg))
            continue

        if row.kong_provider not in MODEL_PROVIDER:
            rep.unsupported.append(
                (row.route, row.plugin,
                 "Kong provider `%s` has no agentgateway provider; its upstream is "
                 "reached through provider-specific indirection (%s) that no CRD "
                 "field expresses" % (row.kong_provider,
                                      ", ".join(sorted(row.options)) or "no options")))
            continue

        if any(k.startswith("iam_") for k in row.auth):
            rep.unsupported.append(
                (row.route, row.plugin,
                 "credential is a VNG IAM access/secret pair exchanged for a token at "
                 "`%s`; agentgateway's auth siblings offer no equivalent exchange"
                 % row.auth.get("iam_auth_url", "?")))
            continue

        if row.route_type not in ROUTE_TYPE_PATH:
            rep.unsupported.append(
                (row.route, row.plugin, "unknown Kong route_type `%s`" % row.route_type))
            continue

        if row.route_type == "llm/v1/generateContent" and row.kong_provider != "gemini":
            rep.unsupported.append(
                (row.route, row.plugin,
                 "route_type llm/v1/generateContent on provider `%s` upstream `%s` is "
                 "not an LLM route type agentgateway knows"
                 % (row.kong_provider, row.upstream_url)))
            continue

        # --- feature-level refusals (row survives, feature does not) ------
        for key, desc in row.unsupported_features:
            rep.feature_scan[key] += 1
            rep.unsupported.append(
                (row.route, row.plugin,
                 "`%s` (%s) dropped -- no agentgateway equivalent; the rest of the "
                 "row is still emitted" % (key, desc)))

        if row.kong_model_name is None:
            rep.inferred.append((row.route, row.plugin, row.model_literal))

        for key, desc in row.lossy_options:
            rep.loss("Kong `model.options` with no agentgateway equivalent",
                     "`%s` on %s -- %s" % (key, tag, desc))
        if row.cfg.get("system_prompt"):
            rep.loss("`system_prompt` injection is not reproducible",
                     tag + " -- agentgateway `policies.transformations` sets body "
                           "*fields* via CEL; it cannot prepend a system message")
        if row.cfg.get("transform"):
            rep.loss("`transform` request mutation is not reproduced",
                     tag + " -- Kong injects literal request headers here (one of "
                           "them a plaintext provider key); not carried across")
        if not ("header_value" in row.auth or "param_value" in row.auth):
            rep.loss("Rows emitted with no credential at all",
                     tag + " -- Kong supplies this upstream's credential by some "
                           "means other than `auth.header_value`/`auth.param_value` "
                           "(here: %s). The emitted model/backend carries NO "
                           "`policies.auth`, so it will fail upstream authentication "
                           "until one is attached by hand."
                     % (", ".join(sorted(row.auth)) or "no `auth` block at all"))
        if row.cfg.get("failover_on"):
            rep.loss("`failover_on` HTTP-status failover degrades to health-based failover",
                     tag + " -- Kong retries the next target on status "
                     + ",".join(str(x) for x in row.cfg["failover_on"])
                     + "; agentgateway priority groups fail over on endpoint health, "
                       "not per-request status")
        keep.append(row)

    for name in unreachable:
        cfg = (ai_proxy[name].get("config") or {})
        kind = "preserve" if name.endswith("-preserve-model") else "orphan"
        if kind == "preserve":
            rep.refused_identity.append(
                ("-", name,
                 "`preserve` plugin: route_type `%s`, no route attachment and no "
                 "addressable model -- nothing to key on"
                 % (cfg.get("route_type") or "?")))
        rep.unreachable.append(
            (name, "ai-proxy plugin attached to no HTTPRoute (%s)" % kind))
    return keep


# --------------------------------------------------------------------------
# grouping into logical models / backends
# --------------------------------------------------------------------------

class Backend(object):
    def __init__(self, name, rows, base, canonical_base, kong_provider):
        self.name = name
        self.rows = rows
        self.base = base
        self.canonical_base = canonical_base
        self.kong_provider = kong_provider


def group(rows, rep):
    """Group kept rows by logical model, then by distinct upstream base."""
    keys = collections.OrderedDict()
    for row in rows:
        k = (row.tenant, row.vendor, row.model_literal)
        keys.setdefault(k, []).append(row)

    backends, models = [], []
    used_names = set()

    def unique(name):
        base = name[:253]
        n, cand = 1, base
        while cand in used_names:
            n += 1
            cand = "%s-%d" % (base[:249], n)
        used_names.add(cand)
        return cand

    # bare-name collisions across vendors make an unprefixed match.model
    # ambiguous; agentgateway's resolve_concrete_model() is a first-match find,
    # so the loser would be silently unreachable.
    bare_owners = collections.defaultdict(set)
    for (tenant, vendor, model) in keys:
        if tenant is None:
            bare_owners[model].add(vendor)

    for (tenant, vendor, model), krows in keys.items():
        tag = "%s/%s" % (vendor, model)
        by_base = collections.OrderedDict()
        for row in krows:
            if row.upstream_url:
                base, canonical = derive_base(row.upstream_url)
            else:
                base, canonical = PROVIDER_DEFAULT_BASE.get(row.kong_provider), True
            by_base.setdefault((base, canonical), []).append(row)

        prefix = ("t%s-" % tenant) if tenant else ""
        for idx, ((base, canonical), brows) in enumerate(by_base.items()):
            nm = unique("legacy-%s%s-%s" % (prefix, slug(vendor), slug(model))
                        + ("" if idx == 0 else "-%d" % (idx + 1)))
            backends.append(Backend(nm, brows, base, canonical, brows[0].kong_provider))

        # --- canonical (AgentgatewayModel) eligibility --------------------
        if tenant is not None:
            rep.loss("Tenant-pinned routes get no canonical AgentgatewayModel",
                     "%s (tenant user-%s, %d row(s)) -- the Kong path pins the "
                     "tenant, so the credential is that tenant's. A body-`model`-"
                     "routed AgentgatewayModel carries no tenant, so publishing one "
                     "would expose this tenant's key to every caller. Legacy surface "
                     "only." % (tag, tenant, len(krows)))
            continue
        if len(by_base) > 1:
            rep.loss("Models with more than one upstream refused on the canonical surface",
                     "%s resolves to %d different upstreams (%s) depending on the "
                     "verb; an AgentgatewayModel has exactly one baseURL and there is "
                     "no per-path override, so no canonical model is emitted. Legacy "
                     "surface only." % (tag, len(by_base),
                                        ", ".join(sorted(str(b) for b, _ in by_base))))
            continue
        (base, canonical), brows = list(by_base.items())[0]
        if not canonical:
            rep.loss("Models on a non-derivable upstream path refused on the canonical surface",
                     "%s -> `%s` is not a path agentgateway derives from a route type. "
                     "AgentgatewayBackend can force it with `path`, AgentgatewayModel "
                     "cannot. Legacy surface only." % (tag, base))
            continue

        upstream_names = set(r.kong_model_name or r.model_literal for r in brows)
        if len(upstream_names) > 1:
            rep.loss("Models with an inconsistent upstream model name refused",
                     "%s sends %s upstream depending on the verb; a single "
                     "AgentgatewayModel has one `transformations` entry."
                     % (tag, sorted(upstream_names)))
            continue
        upstream_name = upstream_names.pop()

        providers = set(r.kong_provider for r in brows)
        if len(providers) > 1:
            rep.loss("Models with an inconsistent provider refused",
                     "%s is configured as %s depending on the verb" % (tag, sorted(providers)))
            continue

        extra_paths = sorted({r.verb for r in brows} & NON_BUILTIN_PATHS)
        auth_rows = [r for r in brows if r.auth]
        auth = auth_rows[0] if auth_rows else None

        match_names = ["%s/%s" % (vendor, model)]
        if len(bare_owners[model]) == 1:
            match_names.append(model)
        else:
            rep.loss("Bare (unprefixed) model names skipped where two vendors collide",
                     "`%s` is served by vendors %s. `match.model` is matched verbatim "
                     "and resolve_concrete_model() is a first-match find, so a bare "
                     "`%s` CR would silently hide one of them. Only the exact "
                     "`<vendor>/<model>` forms are emitted."
                     % (model, sorted(bare_owners[model]), model))

        for mm in match_names:
            nm = unique("m-" + slug(mm))
            models.append({
                "name": nm, "match": mm, "vendor": vendor, "model": model,
                "provider": MODEL_PROVIDER[brows[0].kong_provider],
                "base": base, "upstream_name": upstream_name,
                "auth": auth, "paths": extra_paths, "rows": brows,
            })
    return backends, models


# --------------------------------------------------------------------------
# credentials
# --------------------------------------------------------------------------

class Credentials(object):
    """Deduplicates Kong's inline plaintext credentials into named Secrets.

    The credential VALUE never leaves this object: it is used only as a
    dictionary key so that two routes sharing a key share a Secret. Names are
    assigned by first-use order, never derived from the value.
    """

    def __init__(self):
        self._by_value = collections.OrderedDict()
        self._n = 0

    def ref(self, auth, consumer):
        if not auth:
            return None
        if "header_value" in auth:
            value, loc = auth["header_value"], ("header", auth.get("header_name", "Authorization"))
        elif "param_value" in auth:
            if auth.get("param_location") != "query":
                return None
            value, loc = auth["param_value"], ("queryParameter", auth.get("param_name", "key"))
        else:
            return None
        entry = self._by_value.get(value)
        if entry is None:
            self._n += 1
            entry = {"name": "maas-cred-%02d" % self._n, "loc": loc, "consumers": []}
            self._by_value[value] = entry
        entry["consumers"].append(consumer)
        return entry

    def secrets(self):
        return list(self._by_value.values())


# --------------------------------------------------------------------------
# YAML emission
# --------------------------------------------------------------------------

def emit_auth(entry, indent):
    """policies.auth block. `key`/`secretRef`/`passthrough`/`aws`/`azure`/`gcp`/
    `oauthTokenExchange` are CEL-enforced mutually exclusive siblings, so only
    `secretRef` is ever set. `secretRef.key` names the key WITHIN the Secret."""
    p = " " * indent
    kind, name = entry["loc"]
    out = [p + "auth:"]
    out.append(p + "  secretRef:")
    out.append(p + "    name: " + yq(entry["name"]))
    out.append(p + "    key: key")
    # Kong writes the credential into the header verbatim. An explicit location
    # reproduces that exactly; the default injection would prepend `Bearer `,
    # which is wrong for the `Basic ` and `x-api-key` credentials in this
    # surface.
    out.append(p + "  location:")
    if kind == "header":
        out.append(p + "    header:")
        out.append(p + "      name: " + yq(name))
    else:
        out.append(p + "    queryParameter:")
        out.append(p + "      name: " + yq(name))
    return out


def emit_models(models, creds, ns, gw_name, gw_ns):
    out = ["# GENERATED by pilot/gen/generate.py -- do not edit by hand.",
           "#",
           "# Layer 1, canonical surface. AgentgatewayModel routes on the request",
           "# body's `model` field over the built-in canonical path list.",
           "#",
           "# TWO CRs PER LOGICAL MODEL. Kong's ai-routing pluginserver strips a",
           "# vendor prefix from the body model (`^[^/]+/(.+)$`) before dispatching,",
           "# so clients address `<vendor>/<model>`. agentgateway matches",
           "# `spec.match.model` VERBATIM and has no rewrite, so the prefixed form is",
           "# emitted as an exact match, plus a bare-name CR wherever that name is",
           "# unambiguous. See out/report.md for why `*/<model>` is NOT used.",
           "#",
           "# No `routes` / route-type config is emitted anywhere: ModelPolicies has",
           "# no such field and an AgentgatewayPolicy cannot target a model.",
           "# See pilot/gen/README.md."]
    for m in models:
        out.append("---")
        out.append("apiVersion: agentgateway.dev/v1alpha1")
        out.append("kind: AgentgatewayModel")
        out.append("metadata:")
        out.append("  name: " + yq(m["name"]))
        out.append("  namespace: " + yq(ns))
        out.append("spec:")
        out.append("  parentRefs:")
        out.append("    - group: gateway.networking.k8s.io")
        out.append("      kind: Gateway")
        out.append("      name: " + yq(gw_name))
        if gw_ns and gw_ns != ns:
            out.append("      namespace: " + yq(gw_ns))
        out.append("  match:")
        out.append("    model: " + yq(m["match"]))
        if m["paths"]:
            out.append("    # Outside the built-in model_router_matches() list, so the")
            out.append("    # path has to be opened explicitly. It resolves to")
            out.append("    # RouteType::Passthrough, which reports no token usage --")
            out.append("    # recorded in out/report.md under Fidelity losses.")
            out.append("    paths:")
            for p in m["paths"]:
                out.append("      - " + yq(p))
        out.append("  provider: " + m["provider"])
        if m["base"]:
            out.append("  baseURL: " + yq(m["base"]))
        out.append("  visibility: Public")
        policy = []
        if m["auth"] is not None:
            entry = creds.ref(m["auth"].auth, "model/" + m["name"])
            if entry:
                policy.extend(emit_auth(entry, 4))
        if m["upstream_name"] != m["match"]:
            policy.append("    # Kong sent `%s` upstream while the client addresses"
                          % m["upstream_name"])
            policy.append("    # `%s`. CEL string literal, hence the inner quotes."
                          % m["match"])
            policy.append("    transformations:")
            policy.append("      - field: model")
            policy.append("        expression: " + yq("'%s'" % m["upstream_name"]))
        if policy:
            out.append("  policies:")
            out.extend(policy)
    return "\n".join(out) + "\n"


def emit_provider_block(row, base, canonical, kong_provider, name, creds, indent, consumer):
    p = " " * indent
    out = [p + "- name: " + yq(name)]
    out.append(p + "  " + BACKEND_PROVIDER[kong_provider] + ":")
    upstream = row.kong_model_name or row.model_literal
    if upstream:
        out.append(p + "    # Provider-level model-name override: what Kong sent upstream.")
        out.append(p + "    model: " + yq(upstream))
    parts = split_base_url(base) if base else None
    policy = []
    if parts:
        scheme, host, port, path = parts
        out.append(p + "  host: " + yq(host))
        # CEL: "host and port must be set together".
        out.append(p + "  port: %d" % port)
        if canonical:
            # set_default_path() short-circuits when a host override is present
            # and no prefix is set, forwarding the inbound path verbatim.
            out.append(p + "  pathPrefix: " + yq(path))
        else:
            # Not a path agentgateway derives from a route type: force it.
            out.append(p + "  path: " + yq(path))
        if scheme == "https":
            # A host override bypasses the provider connector defaults, which is
            # where TLS origination comes from; without this the gateway speaks
            # plaintext to :443 and the upstream resets.
            policy.append(p + "    tls: {}")
    entry = creds.ref(row.auth, consumer)
    if entry:
        policy.extend(emit_auth(entry, indent + 4))
    if policy:
        out.append(p + "  policies:")
        out.extend(policy)
    return out


def emit_backends(backends, creds, ns, rep):
    out = ["# GENERATED by pilot/gen/generate.py -- do not edit by hand.",
           "#",
           "# Layer 2, legacy URL surface. AgentgatewayModel cannot serve these",
           "# paths (model_router_matches() is a fixed canonical list), so each",
           "# legacy HTTPRoute rule names an AgentgatewayBackend in backendRefs.",
           "#",
           "# ONE LOGICAL MODEL PER BACKEND: spec.ai.groups[].providers[] is a random",
           "# power-of-two-choices pool that ignores the requested model name, so the",
           "# route regex pins the model segment and each backend serves one model.",
           "# `groups` ARE priority ordered, so a Kong `fallbacks` entry becomes a",
           "# lower-priority group."]
    for b in backends:
        row = b.rows[0]
        out.append("---")
        out.append("apiVersion: agentgateway.dev/v1alpha1")
        out.append("kind: AgentgatewayBackend")
        out.append("metadata:")
        out.append("  name: " + yq(b.name))
        out.append("  namespace: " + yq(ns))
        out.append("spec:")
        out.append("  ai:")
        out.append("    groups:")
        out.append("      - providers:")
        out.extend(emit_provider_block(row, b.base, b.canonical_base, b.kong_provider,
                                       "primary", creds, 10, "backend/" + b.name))
        for i, fb in enumerate(row.cfg.get("fallbacks") or []):
            fm = fb.get("model") or {}
            fprov = fm.get("provider")
            if fprov not in BACKEND_PROVIDER:
                rep.unsupported.append(
                    (row.route, row.plugin,
                     "fallback #%d uses Kong provider `%s`, which has no agentgateway "
                     "provider; the fallback is dropped" % (i + 1, fprov)))
                continue
            furl = (fm.get("options") or {}).get("upstream_url")
            if not furl:
                fbase, fcanon = PROVIDER_DEFAULT_BASE.get(fprov), True
            else:
                fbase, fcanon = derive_base(furl)

            class _FRow(object):
                pass
            fr = _FRow()
            fr.kong_model_name = fm.get("name")
            fr.model_literal = row.model_literal
            fr.auth = fb.get("auth") or {}
            out.append("      # Kong fallback #%d -> lower-priority group." % (i + 1))
            out.append("      - providers:")
            out.extend(emit_provider_block(fr, fbase, fcanon, fprov,
                                           "fallback-%d" % (i + 1), creds, 10,
                                           "backend/%s/fallback-%d" % (b.name, i + 1)))
    return "\n".join(out) + "\n"


def emit_httproutes(backends, ns, gw_name, gw_ns, rep):
    rules = []
    for b in backends:
        for row in b.rows:
            if row.route_type == "llm/v1/generateContent":
                rep.loss("Legacy `/v1/generateContent` URLs are not reproduced",
                         "%s / %s -- proven in pilot/02-httproutes.yaml: neither the "
                         "model router nor an AI backend knows `generateContent`, so a "
                         "Google-native body is fed to the chat-completions parser and "
                         "rejected. The canonical surface still opens the path via "
                         "`match.paths`." % (row.route, row.plugin))
                continue
            rules.append((row, b.name))

    out = ["# GENERATED by pilot/gen/generate.py -- do not edit by hand.",
           "#",
           "# Legacy Kong URLs, reproduced verbatim as RegularExpression matches and",
           "# anchored with `^`. Every rule carries BOTH a backendRef and a URLRewrite:",
           "# a URLRewrite filter does not re-run route matching, so a rule without a",
           "# backendRef dead-ends with `500 no valid backends`, and without the",
           "# rewrite the upstream never sees a provider-native path.",
           "#",
           "# Chunked at %d rules per HTTPRoute (spec.rules maxItems)."
           % MAX_RULES_PER_HTTPROUTE]
    for n in range(0, len(rules), MAX_RULES_PER_HTTPROUTE):
        chunk = rules[n:n + MAX_RULES_PER_HTTPROUTE]
        out.append("---")
        out.append("apiVersion: gateway.networking.k8s.io/v1")
        out.append("kind: HTTPRoute")
        out.append("metadata:")
        out.append("  name: " + yq("legacy-ai-%02d" % (n // MAX_RULES_PER_HTTPROUTE + 1)))
        out.append("  namespace: " + yq(ns))
        out.append("spec:")
        out.append("  parentRefs:")
        out.append("    - group: gateway.networking.k8s.io")
        out.append("      kind: Gateway")
        out.append("      name: " + yq(gw_name))
        if gw_ns and gw_ns != ns:
            out.append("      namespace: " + yq(gw_ns))
        out.append("  rules:")
        for row, backend in chunk:
            out.append("    # %s -> %s" % (row.route, row.plugin))
            out.append("    - matches:")
            out.append("        - method: POST")
            out.append("          path:")
            out.append("            type: RegularExpression")
            out.append("            value: " + yq("^" + row.path_value))
            out.append("      filters:")
            out.append("        - type: URLRewrite")
            out.append("          urlRewrite:")
            out.append("            path:")
            out.append("              type: ReplaceFullPath")
            out.append("              replaceFullPath: " + yq(ROUTE_TYPE_PATH[row.route_type]))
            out.append("      backendRefs:")
            out.append("        - group: agentgateway.dev")
            out.append("          kind: AgentgatewayBackend")
            out.append("          name: " + yq(backend))
    return "\n".join(out) + "\n", len(rules), (len(rules) + MAX_RULES_PER_HTTPROUTE - 1) // MAX_RULES_PER_HTTPROUTE


def emit_secrets(creds, ns):
    out = ["# GENERATED by pilot/gen/generate.py -- do not edit by hand.",
           "#",
           "# CREDENTIAL VALUES ARE NOT COPIED OUT OF KONG. Kong stores provider keys",
           "# inline as plaintext `config.auth.header_value` / `config.auth.param_value`.",
           "# Duplicating them into a generated file would spread the plaintext to a",
           "# second place on disk, so each Secret below is a SHELL: the value is the",
           "# literal string REPLACE_ME and the annotation names the exact Kong field",
           "# to copy it from. Fill these in out-of-band (sealed-secrets, ESO, or by",
           "# hand) before applying.",
           "#",
           "# `secretRef.key` on the model/backend names the key WITHIN the Secret,",
           "# which is `key` for every Secret here."]
    for e in creds.secrets():
        out.append("---")
        out.append("apiVersion: v1")
        out.append("kind: Secret")
        out.append("metadata:")
        out.append("  name: " + yq(e["name"]))
        out.append("  namespace: " + yq(ns))
        out.append("  annotations:")
        out.append("    parity.maas/source-field: " + yq(
            "KongPlugin config.auth.%s" % ("header_value" if e["loc"][0] == "header"
                                           else "param_value")))
        out.append("    parity.maas/injected-as: " + yq(
            "%s %s" % (e["loc"][0], e["loc"][1])))
        out.append("    parity.maas/consumers: " + yq(", ".join(sorted(set(e["consumers"])))))
        out.append("type: Opaque")
        out.append("stringData:")
        out.append("  key: REPLACE_ME")
    return "\n".join(out) + "\n"


# --------------------------------------------------------------------------
# report
# --------------------------------------------------------------------------

def emit_report(rep, ns):
    L = ["# Kong MaaS v2 -> agentgateway parity: refusals and fidelity losses",
         "",
         "Generated by `pilot/gen/generate.py` from live namespace `%s`." % ns,
         "Every row below is a place where the Kong surface says something the",
         "agentgateway CRDs cannot say. Nothing here was guessed or approximated.",
         ""]

    L.append("## Unsupported")
    L.append("")
    L.append("Kong config with no agentgateway equivalent. Where the line says")
    L.append("`dropped`, the row is still emitted without that feature; otherwise the")
    L.append("whole row is refused.")
    L.append("")
    L.append("Feature keys scanned across every joined row, with the number of rows "
             "carrying each: "
             + ", ".join("`%s` %d" % (k, v) for k, v in rep.feature_scan.items())
             + ".")
    L.append("")
    if rep.unsupported:
        for route, plugin, why in sorted(rep.unsupported):
            L.append("- `%s` / `%s` -- %s" % (route, plugin, why))
    else:
        L.append("- (none)")
    L.append("")

    L.append("## Refused --- no model identity")
    L.append("")
    L.append("Nothing to key an `AgentgatewayModel` or a per-model")
    L.append("`AgentgatewayBackend` on, so nothing is emitted.")
    L.append("")
    if rep.refused_identity:
        for route, plugin, why in sorted(rep.refused_identity):
            L.append("- `%s` / `%s` -- %s" % (route, plugin, why))
    else:
        L.append("- (none)")
    L.append("")

    L.append("## Unreachable --- configured but attached to no route")
    L.append("")
    L.append("ai-proxy plugins that no HTTPRoute references. They are dead in Kong")
    L.append("today; nothing is emitted for them.")
    L.append("")
    if rep.unreachable:
        for plugin, why in sorted(rep.unreachable):
            L.append("- `%s` -- %s" % (plugin, why))
    else:
        L.append("- (none)")
    L.append("")

    L.append("## Model name inferred from route path")
    L.append("")
    L.append("`config.model.name` is unset, so the model identity was taken from the")
    L.append("model segment of the HTTPRoute path -- which is what the client actually")
    L.append("addresses. Verify each of these against the upstream's own model list.")
    L.append("")
    if rep.inferred:
        for route, plugin, name in sorted(rep.inferred):
            L.append("- `%s` / `%s` -> `%s`" % (route, plugin, name))
    else:
        L.append("- (none)")
    L.append("")

    L.append("## Models that cannot carry guardrails")
    L.append("")
    L.append("Models whose route type resolves to `Detect`, for which")
    L.append("`InputFormat::supports_prompt_guard()` is false and `get_messages()` is")
    L.append("`unimplemented!()` -- a guardrail on such a model is accepted and")
    L.append("silently never runs. See `pilot/gen/README.md`.")
    L.append("")
    if rep.no_guardrail_models:
        for m in sorted(rep.no_guardrail_models):
            L.append("- `%s`" % m)
    else:
        L.append("- (none) --- no CRD field maps any path to `Detect` today, so this")
        L.append("  list is necessarily empty. It becomes load-bearing the moment a")
        L.append("  `routes` surface lands on `ModelPolicies`.")
    L.append("")

    L.append("## Fidelity losses")
    L.append("")
    for heading, items in rep.losses.items():
        L.append("### " + heading)
        L.append("")
        for it in sorted(set(items)):
            L.append("- " + it)
        L.append("")

    L.append("### Structural losses that apply to the whole output")
    L.append("")
    L.append("- **No route-type configuration is emitted anywhere.** `ModelPolicies`")
    L.append("  has no `routes` field and an `AgentgatewayPolicy` cannot target a")
    L.append("  model, so there is nowhere to put it. `/v1/completions` and")
    L.append("  `/v1/generateContent` are therefore opened with `spec.match.paths` and")
    L.append("  resolve through the `\"*\"` wildcard to `RouteType::Passthrough`, which")
    L.append("  forwards the body untranslated and **extracts no token usage**. Every")
    L.append("  request on those two paths is unmetered -- a direct miss on FR-7.1.")
    L.append("- **No weighting on the legacy surface.** `AgentgatewayBackend` has no")
    L.append("  weight field, so a Kong canary split cannot be reproduced there. A")
    L.append("  weighted split belongs on the canonical surface as a virtual")
    L.append("  `AgentgatewayModel` (see `pilot/06-model-deepseek.yaml`); this")
    L.append("  generator does not synthesise one, because guessing which of two")
    L.append("  upstreams is the canary is exactly the kind of invention it refuses.")
    L.append("- **Two model CRs per logical model.** Kong strips a vendor prefix from")
    L.append("  the body `model` before dispatching; agentgateway matches")
    L.append("  `spec.match.model` verbatim and one CR carries one match. So each")
    L.append("  logical model needs an exact `<vendor>/<model>` CR plus a bare")
    L.append("  `<model>` CR, roughly doubling the canonical CR count versus Kong.")
    L.append("  **The prefixed CR is an exact match, not the `*/<model>` wildcard the")
    L.append("  design contract proposed.** `*/<model>` is admissible under the CEL")
    L.append("  rule, but it matches *every* vendor prefix, and this surface really")
    L.append("  does serve one bare name from two vendors. Since")
    L.append("  `resolve_concrete_model()` is a first-match `find()`, a wildcard CR")
    L.append("  would silently swallow the other vendor's traffic and send it to the")
    L.append("  wrong upstream with the wrong credential. An exact prefix cannot.")
    L.append("- **Timeouts, `max_request_body_size`, `logging` and")
    L.append("  `response_streaming` are not carried.** They are gateway- or")
    L.append("  listener-level concerns in agentgateway, not per-model ones; set them")
    L.append("  on the Gateway/listener (see `pilot/18-policy-buffer.yaml`).")
    L.append("- **No guardrail webhook is attached to any model.** The only")
    L.append("  keyword-config shape in the contract's fixtures is")
    L.append("  `external_keyword_service_url`, which that edge denies, so attaching")
    L.append("  the webhook would reject every guardrailed request; and the model the")
    L.append("  webhook sees is client-controlled through an unauthenticated")
    L.append("  `X-Model` header. Both preconditions are unmet.")
    L.append("- **`/v1/models` is not emitted.** Kong serves it from a dedicated")
    L.append("  HTTPRoute plus the catalog pluginserver; `AgentgatewayModel` serves")
    L.append("  `/v1/models` natively from the set of `Public` models, so there is")
    L.append("  nothing to translate.")
    L.append("- **Secrets are emitted as shells, not values.** See the header of")
    L.append("  `out/secrets.yaml`.")
    L.append("- **A generator-supplied default base URL is used for Kong provider")
    L.append("  `deepseek`** (`%s`), because the agentgateway backend CRD has no"
             % PROVIDER_DEFAULT_BASE["deepseek"])
    L.append("  `deepseek` provider key and `openai` with no host would silently go to")
    L.append("  api.openai.com. Stated here rather than left implicit.")
    L.append("")

    L.append("## Counts")
    L.append("")
    for k, v in rep.counts.items():
        L.append("- %s: %s" % (k, v))
    L.append("")
    return "\n".join(L)


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--kubeconfig", required=True)
    ap.add_argument("--namespace", required=True,
                    help="Kong namespace to read (READ-ONLY)")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--target-namespace", default=None,
                    help="namespace the generated CRs go in (default: <namespace>-agw)")
    ap.add_argument("--gateway-name", default="maas-v2-agw")
    ap.add_argument("--gateway-namespace", default=None)
    args = ap.parse_args()

    target_ns = args.target_namespace or (args.namespace + "-agw")
    rep = Report()

    ai_proxy, rows, unreachable, no_plugin_routes, unparsed = extract(
        args.kubeconfig, args.namespace)
    for rname, pname, val in unparsed:
        rep.refused_identity.append(
            (rname, pname, "HTTPRoute path `%s` does not have the "
                           "`{vendor}/{model}/v1/{verb}` shape this translator "
                           "understands" % val))

    kept = classify(rows, unreachable, ai_proxy, rep)
    backends, models = group(kept, rep)

    creds = Credentials()
    os.makedirs(args.out_dir, exist_ok=True)

    # backends first so every credential is registered before secrets.yaml
    backends_yaml = emit_backends(backends, creds, target_ns, rep)
    models_yaml = emit_models(models, creds, target_ns,
                              args.gateway_name, args.gateway_namespace)
    routes_yaml, n_rules, n_routes = emit_httproutes(
        backends, target_ns, args.gateway_name, args.gateway_namespace, rep)
    secrets_yaml = emit_secrets(creds, target_ns)

    rep.counts["ai-proxy plugins read"] = len(ai_proxy)
    rep.counts["(plugin, route) rows joined"] = len(rows)
    rep.counts["rows kept"] = len(kept)
    rep.counts["rows refused"] = len(rows) - len(kept)
    rep.counts["  refused: no model identity"] = sum(
        1 for r in rep.refused_identity if r[0] != "-")
    rep.counts["  refused: unsupported"] = (len(rows) - len(kept)) - sum(
        1 for r in rep.refused_identity if r[0] != "-")
    rep.counts["ai-proxy plugins attached to no route"] = len(unreachable)
    rep.counts["HTTPRoutes carrying no ai-proxy plugin"] = len(no_plugin_routes)
    rep.counts["AgentgatewayModel CRs emitted"] = len(models)
    rep.counts["  of which exact `<vendor>/<model>`"] = sum(1 for m in models if "/" in m["match"])
    rep.counts["  of which bare `<model>`"] = sum(1 for m in models if "/" not in m["match"])
    rep.counts["AgentgatewayBackend CRs emitted"] = len(backends)
    rep.counts["legacy HTTPRoute rules emitted"] = n_rules
    rep.counts["legacy HTTPRoute CRs emitted"] = n_routes
    rep.counts["Secret shells emitted"] = len(creds.secrets())
    rep.counts["distinct upstream_url values seen"] = len(
        {r.upstream_url for r in rows if r.upstream_url})

    def write(name, body):
        with open(os.path.join(args.out_dir, name), "w") as fh:
            fh.write(body)

    write("backends.yaml", backends_yaml)
    write("models.yaml", models_yaml)
    write("httproutes.yaml", routes_yaml)
    write("secrets.yaml", secrets_yaml)
    write("report.md", emit_report(rep, args.namespace))

    for k, v in rep.counts.items():
        print("%-45s %s" % (k, v))
    print("\nwrote models.yaml backends.yaml httproutes.yaml secrets.yaml report.md "
          "-> %s" % args.out_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
