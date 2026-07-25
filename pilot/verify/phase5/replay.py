#!/usr/bin/env python3
"""Fire the Phase 5 corpus at a gateway and record a comparable result file.

Deliberately gateway-agnostic: the same corpus can be run against the pilot
(agentgateway) and against Kong, and the two result files diffed. Only the
--host, --surface and --key differ between the arms. Nothing here knows which
gateway it is talking to, so neither arm can be accidentally favoured.

    ./replay.py --host 116.118.88.175.nip.io --surface canonical \
                --key "$PILOT_API_KEY" --out pilot-canonical.json

SURFACES
  canonical  POST the canonical path (/v1/chat/completions etc.) with the model
             in the body. Skips corpus entries with no canonical equivalent and
             records them as such — an uncovered surface is a RESULT, not an
             omission.
  legacy     POST the original Kong path verbatim. This is the arm that proves
             a real client needs no change.

COST SAFETY
Every request caps output tokens (see build_corpus.py) and the corpus is fired
once, sequentially by default. --limit and --route-type exist so a smoke run
costs a handful of requests rather than 254. --dry-run prints what would be
sent and exits without any network call.
"""

import argparse
import json
import ssl
import sys
import time
import urllib.error
import urllib.request

# The pilot gateway presents a self-signed certificate on a nip.io host, so
# verification is disabled for BOTH arms. Doing it for one arm only would make
# the latency comparison dishonest.
INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


def fire(host, path, body, key, timeout):
    url = f"https://{host}{path}"
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    if key:
        req.add_header("Authorization", f"Bearer {key}")

    started = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=INSECURE) as resp:
            payload = resp.read(65536)
            return resp.status, dict(resp.headers), payload, time.monotonic() - started
    except urllib.error.HTTPError as e:
        # A 4xx/5xx is a RESULT, not an error: the status code is precisely
        # what the comparison is about.
        return e.code, dict(e.headers), e.read(65536), time.monotonic() - started
    except Exception as e:
        return None, {}, str(e).encode(), time.monotonic() - started


def usage_from(payload):
    """Pull token counts out of a response body, tolerating provider variance."""
    try:
        doc = json.loads(payload)
    except Exception:
        return None
    u = doc.get("usage") or doc.get("usageMetadata")
    if not isinstance(u, dict):
        return None
    return {
        "prompt": u.get("prompt_tokens", u.get("input_tokens", u.get("promptTokenCount"))),
        "completion": u.get(
            "completion_tokens", u.get("output_tokens", u.get("candidatesTokenCount"))
        ),
        "total": u.get("total_tokens", u.get("totalTokenCount")),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--host", required=True)
    ap.add_argument("--surface", choices=["canonical", "legacy"], required=True)
    ap.add_argument("--key", default="", help="bearer token; omit to test the deny path")
    ap.add_argument("--out", help="required unless --dry-run")
    ap.add_argument("--timeout", type=float, default=90.0)
    ap.add_argument("--limit", type=int, default=0, help="0 = whole corpus")
    ap.add_argument("--route-type", default="", help="filter, e.g. llm/v1/chat")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    if not args.dry_run and not args.out:
        ap.error("--out is required unless --dry-run")

    corpus = json.load(open(args.corpus))["requests"]
    if args.route_type:
        corpus = [c for c in corpus if c["route_type"] == args.route_type]

    planned, uncovered = [], []
    for c in corpus:
        if args.surface == "canonical":
            path = c["canonical_path"]
            if not path:
                uncovered.append(c["id"])
                continue
        else:
            path = c["legacy_path"]
        planned.append((c, path))

    if args.limit:
        planned = planned[: args.limit]

    print(
        f"host={args.host} surface={args.surface} "
        f"authenticated={'yes' if args.key else 'no'} "
        f"requests={len(planned)} uncovered={len(uncovered)}"
    )
    if args.dry_run:
        for c, path in planned[:20]:
            print(f"  POST {path}  model={c['model']}")
        if len(planned) > 20:
            print(f"  ... and {len(planned) - 20} more")
        return 0

    results = []
    for i, (c, path) in enumerate(planned, 1):
        status, headers, payload, elapsed = fire(
            args.host, path, c["body"], args.key, args.timeout
        )
        results.append(
            {
                "id": c["id"],
                "model": c["model"],
                "provider": c["provider"],
                "route_type": c["route_type"],
                "path": path,
                "status": status,
                "latency_s": round(elapsed, 3),
                "usage": usage_from(payload),
                # Truncated: enough to classify the failure, not enough to make
                # the result file a dump of model output.
                "body_head": payload[:300].decode("utf-8", "replace"),
                "tenant_header": headers.get("X-Tenant-ID"),
            }
        )
        if i % 20 == 0 or i == len(planned):
            print(f"  {i}/{len(planned)}")

    by_status = {}
    for r in results:
        by_status[r["status"]] = by_status.get(r["status"], 0) + 1

    lat = sorted(r["latency_s"] for r in results if r["status"] is not None)
    summary = {
        "host": args.host,
        "surface": args.surface,
        "authenticated": bool(args.key),
        "n": len(results),
        "by_status": {str(k): v for k, v in sorted(by_status.items(), key=lambda x: str(x[0]))},
        "uncovered_ids": uncovered,
        "latency_p50": lat[len(lat) // 2] if lat else None,
        "latency_p95": lat[int(len(lat) * 0.95)] if lat else None,
    }
    json.dump({"summary": summary, "results": results}, open(args.out, "w"), indent=2)

    print()
    print(json.dumps(summary["by_status"], indent=2))
    print(f"p50={summary['latency_p50']}s p95={summary['latency_p95']}s")
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
