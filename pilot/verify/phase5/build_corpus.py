#!/usr/bin/env python3
"""Derive a Phase 5 request corpus from the live Kong configuration.

WHY THIS EXISTS INSTEAD OF A TRAFFIC CAPTURE
--------------------------------------------
The design (Section 11, Phase 5) calls for replaying *captured* Kong traffic.
There is none to capture: the entire available dataplane access log for
user-11377-maas-v2 (3457 lines, ~27h of pod uptime) contains zero requests to
any LLM surface — only /status health checks and internet background scanning.
This is an idle dev gateway.

So the corpus is DERIVED from configuration rather than observed. That is a
weaker basis for latency and cost claims (no real prompt-size or
concurrency distribution) but a STRONGER basis for surface coverage: config
enumerates every route that exists, whereas a capture only shows the ones that
happened to be exercised. Every entry below corresponds to a real, programmed
Kong route serving real tenants.

Input is two read-only snapshots of the Kong namespace:
    kubectl -n user-11377-maas-v2 get httproute  -o json > kong-routes.json
    kubectl -n user-11377-maas-v2 get kongplugin -o json > kong-plugins.json

Nothing in the Kong namespace is written, and no credential is read: the only
plugin fields consulted are model name, provider, and route_type. The `auth`
block, which holds provider API keys, is never touched.
"""

import argparse
import json
import re
import sys

# Kong expresses each route as an anchored regex with an optional tenant
# prefix. Two shapes are in use:
#   /(?:maas/user-[^/]+/)?anthropic/claude-sonnet-4-0/v1/chat/completions$
#   /(?:maas/)?user-11374/openai-compatible/gemini-2\.5-flash/v1/chat/completions$
# Both the with-prefix and without-prefix forms are live, so both are emitted:
# the prefix being optional is a real part of the contract the pilot must
# reproduce, and it is exactly the kind of detail a hand-written corpus would
# miss.
# A real tenant id from the live route set. Kong writes the tenant segment two
# ways — `user-[^/]+` (wildcard is the bare id) and a literal `user-11374` — so
# the numeric and prefixed forms are both needed. Substituting the prefixed
# form into `user-[^/]+` would produce `/maas/user-user-11374/`, a path that
# matches nothing.
SAMPLE_TENANT_ID = "11374"
SAMPLE_TENANT = f"user-{SAMPLE_TENANT_ID}"

# One optional non-capturing group, e.g. `(?:maas/user-[^/]+/)?`. Expanded into
# a present-variant and an absent-variant rather than picked arbitrarily.
OPTIONAL_GROUP = re.compile(r"\(\?:([^()]*)\)\?")

# A single path segment wildcard. Only this one wildcard is understood; any
# other metacharacter makes the route unparseable and it is skipped loudly.
SEGMENT_WILDCARD = "[^/]+"

# The canonical paths on which agentgateway performs body-model routing.
# Source: crates/agentgateway/src/store/binds.rs, model_router_matches().
# This list is HARDCODED in the Rust source and not configurable, which is why
# it is reproduced here rather than read from any config.
AGW_ROUTER_EXACT = {
    "/v1/models",
    "/models",
    "/v1/chat/completions",
    "/v1/messages",
    "/v1/responses",
    "/v1/responses/compact",
    "/v1/images/generations",
    "/v1/images/edits",
    "/v1/images/variations",
    "/v1/embeddings",
    "/v1/rerank",
    "/v2/rerank",
}
AGW_ROUTER_REGEX = re.compile(
    r"^/v(?:[0-9]+|[0-9]+beta[0-9]+)/projects/[^/]+/locations/[^/]+"
    r"/publishers/[^/]+/models/[^/]+:(?:rawPredict|streamRawPredict)$"
)

# Minimal valid request body per Kong route_type. Kept deliberately small and
# uniform so that a latency comparison is not skewed by payload size, and so
# that a run costs as little upstream token spend as possible.
PROMPT = "Reply with the single word OK."


def body_for(route_type, model):
    if route_type in ("llm/v1/chat", "llm/v1/completions"):
        if route_type == "llm/v1/completions":
            return {"model": model, "prompt": PROMPT, "max_tokens": 8}
        return {
            "model": model,
            "messages": [{"role": "user", "content": PROMPT}],
            "max_tokens": 8,
        }
    if route_type == "llm/v1/messages":
        return {
            "model": model,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": PROMPT}],
        }
    if route_type == "llm/v1/responses":
        return {"model": model, "input": PROMPT, "max_output_tokens": 16}
    if route_type == "llm/v1/embeddings":
        return {"model": model, "input": "hello"}
    if route_type == "llm/v1/generateContent":
        return {"contents": [{"parts": [{"text": PROMPT}]}]}
    if route_type == "llm/v1/images/generations":
        return {"model": model, "prompt": "a red square", "n": 1, "size": "1024x1024"}
    if route_type == "llm/v1/rerank":
        return {"model": model, "query": "hello", "documents": ["a", "b"]}
    # "preserve" and anything unrecognised: pass a chat body through untouched.
    return {
        "model": model,
        "messages": [{"role": "user", "content": PROMPT}],
        "max_tokens": 8,
    }


def concrete_paths(regex_value):
    """Turn one anchored Kong route regex into the concrete path(s) it accepts.

    Every optional group is expanded both ways, so a route whose tenant prefix
    is optional yields both live forms. Returns [] for a regex this function
    does not fully understand, rather than guessing — a wrong path would
    silently become a spurious 404 and be misread as a routing failure.
    """
    pattern = regex_value.rstrip("$")
    if not pattern.startswith("/"):
        return []

    variants = _expand_optional_groups(pattern)

    out = []
    for v in variants:
        # `[^/]+` stands for a tenant segment. Order matters: consume the
        # `user-[^/]+` form first so its wildcard gets the bare id.
        v = v.replace(f"user-{SEGMENT_WILDCARD}", SAMPLE_TENANT)
        v = v.replace(SEGMENT_WILDCARD, SAMPLE_TENANT)
        # Kong escapes literal dots in model names (gemini-2\.5-flash).
        v = re.sub(r"\\(.)", r"\1", v)
        if _has_regex_metachars(v):
            continue
        v = "/" + v.lstrip("/")
        if v not in out:
            out.append(v)
    return out


def _expand_optional_groups(pattern):
    """Expand every `(?:...)?` into its present and absent forms."""
    variants = [pattern]
    while True:
        grown = []
        changed = False
        for v in variants:
            m = OPTIONAL_GROUP.search(v)
            if not m:
                grown.append(v)
                continue
            changed = True
            grown.append(v[: m.start()] + m.group(1) + v[m.end() :])
            grown.append(v[: m.start()] + v[m.end() :])
        variants = grown
        if not changed:
            return variants


def _has_regex_metachars(s):
    return any(c in s for c in "()[]?*+|\\")


# The trailing surface segments to strip when recovering a model slug from a
# legacy path, longest first so /v1/chat/completions wins over /completions.
SURFACE_SUFFIXES = [
    "/v1/chat/completions",
    "/v1/images/generations",
    "/v1/generateContent",
    "/v1/embeddings",
    "/v1/completions",
    "/v1/responses",
    "/v1/messages",
    "/v1/rerank",
    "/v2/rerank",
]


def model_from_path(path, route_type):
    """Recover the model slug from a legacy Kong path.

    /maas/user-11374/meta-llama/meta-llama-3-8b/v1/chat/completions
                                ^^^^^^^^^^^^^^^ this
    """
    for suffix in SURFACE_SUFFIXES:
        if path.endswith(suffix):
            head = path[: -len(suffix)]
            break
    else:
        return None
    segments = [s for s in head.split("/") if s]
    return segments[-1] if segments else None


def canonical_for(route_type):
    """The agentgateway canonical path this Kong route_type maps onto."""
    return {
        "llm/v1/chat": "/v1/chat/completions",
        "llm/v1/messages": "/v1/messages",
        "llm/v1/responses": "/v1/responses",
        "llm/v1/embeddings": "/v1/embeddings",
        "llm/v1/images/generations": "/v1/images/generations",
        "llm/v1/rerank": "/v1/rerank",
        # No canonical equivalent — see coverage report.
        "llm/v1/completions": None,
        "llm/v1/generateContent": None,
        "preserve": None,
    }.get(route_type)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--routes", required=True)
    ap.add_argument("--plugins", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    routes = json.load(open(args.routes))["items"]
    plugins = {p["metadata"]["name"]: p for p in json.load(open(args.plugins))["items"]}

    corpus, skipped = [], []
    for r in routes:
        name = r["metadata"]["name"]
        ann = (r["metadata"].get("annotations") or {}).get("konghq.com/plugins", "")
        attached = [p.strip() for p in ann.split(",") if p.strip()]

        # The ai-proxy plugin on the route carries the model identity.
        proxy = next(
            (
                plugins[p]
                for p in attached
                if p in plugins and plugins[p].get("plugin") == "ai-proxy"
            ),
            None,
        )
        if proxy is None:
            skipped.append({"route": name, "why": "no ai-proxy plugin attached"})
            continue

        cfg = proxy.get("config") or {}
        model_cfg = cfg.get("model")
        # A handful of plugins carry `model` as a bare string rather than an
        # object; tolerate both instead of crashing the whole build.
        if isinstance(model_cfg, dict):
            model = model_cfg.get("name")
            provider = model_cfg.get("provider")
        else:
            model, provider = model_cfg, None
        route_type = cfg.get("route_type")
        if not route_type:
            skipped.append({"route": name, "why": "plugin lacks route_type"})
            continue

        for rule in r["spec"].get("rules", []):
            for m in rule.get("matches", []):
                path_spec = m.get("path", {})
                if path_spec.get("type") != "RegularExpression":
                    skipped.append({"route": name, "why": "non-regex path match"})
                    continue
                paths = concrete_paths(path_spec["value"])
                if not paths:
                    skipped.append(
                        {"route": name, "why": f"unparsed regex {path_spec['value']}"}
                    )
                    continue
                for path in paths:
                    # Some ai-proxy plugins pin the upstream by URL and carry
                    # no model.name; Kong then takes the model from the client
                    # body. Recover it from the path, which is how a real
                    # client would name it.
                    effective_model = model or model_from_path(path, route_type)
                    if not effective_model:
                        skipped.append(
                            {"route": name, "why": f"no model name derivable from {path}"}
                        )
                        continue
                    corpus.append(
                        {
                            "id": f"{name}::{path}",
                            "kong_route": name,
                            "method": m.get("method", "POST"),
                            "legacy_path": path,
                            "canonical_path": canonical_for(route_type),
                            "model": effective_model,
                            "model_source": "plugin" if model else "path",
                            "provider": provider,
                            "route_type": route_type,
                            "streaming": cfg.get("response_streaming"),
                            "body": body_for(route_type, effective_model),
                        }
                    )

    out = {
        "source": "derived from live Kong config; no traffic capture exists",
        "kong_routes_seen": len(routes),
        "requests": corpus,
        "skipped": skipped,
    }
    json.dump(out, open(args.out, "w"), indent=2)

    # Coverage summary: which Kong surfaces agentgateway's hardcoded router
    # can serve at all. This is the headline Phase 5 finding.
    by_type = {}
    for c in corpus:
        t = by_type.setdefault(c["route_type"], {"n": 0, "canonical": None})
        t["n"] += 1
        t["canonical"] = c["canonical_path"]

    print(f"corpus: {len(corpus)} requests from {len(routes)} Kong routes")
    print(f"skipped: {len(skipped)}")
    print()
    print(f"{'route_type':<28} {'reqs':>5}  agentgateway canonical path")
    print("-" * 78)
    uncovered = 0
    for t, info in sorted(by_type.items(), key=lambda kv: -kv[1]["n"]):
        canon = info["canonical"]
        if canon and (canon in AGW_ROUTER_EXACT or AGW_ROUTER_REGEX.match(canon)):
            mark = canon
        else:
            mark = "*** NO CANONICAL SURFACE ***"
            uncovered += info["n"]
        print(f"{t:<28} {info['n']:>5}  {mark}")
    print("-" * 78)
    pct = 100.0 * uncovered / len(corpus) if corpus else 0.0
    print(f"uncovered: {uncovered}/{len(corpus)} requests ({pct:.1f}%)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
