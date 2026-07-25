#!/usr/bin/env python3
"""Measure SSE stream realtime-ness: TTFT and inter-chunk gaps.

Exists to answer one question with numbers rather than argument: does putting
an ext_proc service on the response path degrade a live SSE stream?

In agentgateway's `FullDuplexStreamed` mode the upstream body is replaced by a
channel-fed StreamBody (http/ext_proc/buffering.rs:342-355), so every response
chunk round-trips gateway -> processor -> gateway before reaching the client.
Whether that is free or costly is an empirical question about one in-cluster
gRPC hop per chunk. Run this before and after enabling the policy and compare.

What matters for "realtime" is not total duration -- a slower model inflates
that regardless -- but:
  * TTFT      : delay before the first token reaches the client
  * gap_p50   : typical spacing between chunks
  * gap_max   : the worst stall, which is what a user actually notices
  * chunks    : if this drops, chunks are being coalesced, i.e. buffered

A processor that accumulates and flushes at end-of-stream shows up unmistakably
as chunks collapsing toward 1 and gap_max approaching total duration.
"""

import argparse
import json
import ssl
import statistics
import sys
import time
import urllib.request

INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


def one_run(host, key, model, prompt, max_tokens, timeout):
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": True,
            "max_tokens": max_tokens,
        }
    ).encode()
    req = urllib.request.Request(
        f"https://{host}/v1/chat/completions", data=body, method="POST"
    )
    req.add_header("Content-Type", "application/json")
    req.add_header("Authorization", f"Bearer {key}")

    started = time.monotonic()
    stamps, usage_seen = [], None

    with urllib.request.urlopen(req, timeout=timeout, context=INSECURE) as resp:
        # Read line-wise off the raw socket so timing reflects arrival, not a
        # buffered decode. Any client-side buffering here would mask exactly
        # the effect we are trying to measure.
        for raw in resp:
            now = time.monotonic()
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            stamps.append(now)
            payload = line[5:].strip()
            if payload and payload != "[DONE]":
                try:
                    doc = json.loads(payload)
                    if doc.get("usage"):
                        usage_seen = doc["usage"]
                except Exception:
                    pass

    if not stamps:
        return None

    gaps = [b - a for a, b in zip(stamps, stamps[1:])]
    return {
        "ttft": stamps[0] - started,
        "total": stamps[-1] - started,
        "chunks": len(stamps),
        "gap_p50": statistics.median(gaps) if gaps else 0.0,
        "gap_max": max(gaps) if gaps else 0.0,
        "usage_in_stream": usage_seen,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--model", default="gemini-2.5-flash")
    ap.add_argument("--prompt", default="Count slowly from 1 to 40, one number per line.")
    ap.add_argument("--max-tokens", type=int, default=200)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--label", default="run", help="printed with results, e.g. before/after")
    ap.add_argument("--out", help="append one JSON line per invocation")
    args = ap.parse_args()

    runs = []
    for i in range(args.runs):
        try:
            r = one_run(
                args.host, args.key, args.model, args.prompt, args.max_tokens, args.timeout
            )
        except Exception as e:
            print(f"  run {i + 1}: FAILED {e}")
            continue
        if r is None:
            print(f"  run {i + 1}: no SSE frames received")
            continue
        runs.append(r)
        print(
            f"  run {i + 1}: ttft={r['ttft']:.3f}s chunks={r['chunks']:>3} "
            f"gap_p50={r['gap_p50'] * 1000:.1f}ms gap_max={r['gap_max'] * 1000:.1f}ms "
            f"total={r['total']:.3f}s"
        )

    if not runs:
        print("no successful runs")
        return 1

    def agg(field):
        return statistics.median(r[field] for r in runs)

    summary = {
        "label": args.label,
        "runs": len(runs),
        "ttft_median": round(agg("ttft"), 4),
        "chunks_median": agg("chunks"),
        "gap_p50_median_ms": round(agg("gap_p50") * 1000, 2),
        "gap_max_median_ms": round(agg("gap_max") * 1000, 2),
        "total_median": round(agg("total"), 4),
        "usage_in_stream": runs[-1]["usage_in_stream"],
    }
    print()
    print(json.dumps(summary, indent=2))
    if args.out:
        with open(args.out, "a") as f:
            f.write(json.dumps(summary) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
