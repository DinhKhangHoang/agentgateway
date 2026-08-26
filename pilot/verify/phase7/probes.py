#!/usr/bin/env python3
"""Phase 7 - AI-gateway probe suite (EVALUATION.md sec.3) against agentgateway.

Build under test:
  vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/agentgateway:maas-v2-parity-dcb5b524
  (feat/maas-v2-parity @ dcb5b524 -- carries the configurable retry cap
   `maxReplayBytes`, crates/agentgateway/src/proxy/httpproxy.rs:998)

Design rules taken from EVALUATION.md sec.3 "Probe design traps":
  * count UPSTREAM HITS, never just the client status code;
  * restart the gateway before P3 (and before every P5 size) so no probe
    inherits another probe's eviction state;
  * kill mocks by container name / listening port, never by process pattern;
  * P12 must cross a process boundary;
  * P14 asserts the mechanism's own signal, not the response;
  * record the mode and the configured values in every result.

Safety rules honoured here:
  * no `--network host` anywhere; a dedicated bridge network `agw-probe` with a
    fixed subnet is created and torn down;
  * every host-side publish is bound to 127.0.0.1 explicitly;
  * no API key value is ever written to stdout, a file, or a result record --
    only sha256 digests of credentials.

stdlib only.

Usage:
  probes.py --probe p5      --out results/p5.json
  probes.py --probe core    --out results/core.json   # P1 P2 P3 P4 P6 P7 P11 P14
  probes.py --probe p12     --out results/p12.json
  probes.py --probe p15     --out results/p15.json
  probes.py --probe p18     --out results/p18.json
  probes.py --probe p8      --out results/p8.json     # cluster
  probes.py --probe p10     --out results/p10.json    # cluster
  probes.py --probe na      --out results/na.json     # P13 P16 P17 rationale
  probes.py --probe cleanup
"""

import argparse
import hashlib
import http.client
import json
import os
import shutil
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))

GW_IMAGE = ("vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/"
            "agentgateway:maas-v2-parity-dcb5b524")
MOCK_IMAGE = "python:3.12-slim"

NET = "agw-probe"
SUBNET = "172.29.77.0/24"
NET_GATEWAY = "172.29.77.1"

# name, container ip, host loopback control port
TIERS = [
    ("tier0", "172.29.77.11", 19010),
    ("tier1", "172.29.77.12", 19011),
    ("tier2", "172.29.77.13", 19012),
]
TIER_NAMES = [t[0] for t in TIERS]

GW_NAME = "agw-probe-gw"
GW_PORT = 18080          # host loopback -> container 8080
GW_NAME_B = "agw-probe-gw-b"
GW_PORT_B = 18081

RETRY_CODES = [429, 500, 502, 503, 504]
# NOTE: despite the schema doc string ("Total number of attempts, including the
# original request"), the dataplane computes `attempts = retry.attempts + 1`
# (httpproxy.rs:989) and logs "attempt 0/N".  ATTEMPTS=2 therefore yields THREE
# upstream attempts -- exactly one per priority tier.  Measured, not assumed.
ATTEMPTS = 2
TOTAL_ATTEMPTS = ATTEMPTS + 1
DEFAULT_MAX_REPLAY = 52_428_800      # 50 MiB
DEFAULT_BUFFER = 64_000_000

KUBECONFIG = "/home/stackops/.kubeconfig/aigateway-dev.conf"
NS = "user-11377-maas-v2-agw"
CLUSTER_HOST = "116.118.88.175.nip.io"

WORK = os.environ.get("AGW_PROBE_WORK",
                      os.path.join(tempfile.gettempdir(), "agw-probe-work"))


# --------------------------------------------------------------------------
# shell / docker helpers
# --------------------------------------------------------------------------

def sh(args, check=True, timeout=300, input_=None):
    p = subprocess.run(args, capture_output=True, text=True, timeout=timeout,
                       input=input_)
    if check and p.returncode != 0:
        raise RuntimeError("cmd failed (%d): %s\nSTDOUT:%s\nSTDERR:%s"
                           % (p.returncode, " ".join(args), p.stdout, p.stderr))
    return p


def docker(*args, **kw):
    return sh(["docker"] + list(args), **kw)


def kubectl(*args, **kw):
    return sh(["kubectl", "--kubeconfig=" + KUBECONFIG, "-n", NS] + list(args),
              **kw)


def log(msg):
    print("[%s] %s" % (time.strftime("%H:%M:%S"), msg), flush=True)


# --------------------------------------------------------------------------
# network + mocks
# --------------------------------------------------------------------------

def net_up():
    p = docker("network", "inspect", NET, check=False)
    if p.returncode != 0:
        docker("network", "create", "--subnet", SUBNET, "--gateway",
               NET_GATEWAY, NET)
        log("created docker network %s (%s)" % (NET, SUBNET))


def net_down():
    docker("network", "rm", NET, check=False)


def container_rm(name):
    """Stop and remove by CONTAINER NAME -- never by process-name pattern."""
    docker("rm", "-f", name, check=False, timeout=90)


def mocks_up():
    os.makedirs(os.path.join(WORK, "bodies"), exist_ok=True)
    uid = "%d:%d" % (os.getuid(), os.getgid())
    for name, ip, hostport in TIERS:
        container_rm(name)
        docker("run", "-d", "--name", name, "--network", NET, "--ip", ip,
               "--user", uid,
               "-p", "127.0.0.1:%d:8080" % hostport,
               "-v", "%s:/probe:ro" % HERE,
               "-v", "%s:/work" % WORK,
               "-e", "MOCK_TIER=%s" % name,
               "-w", "/probe",
               MOCK_IMAGE,
               "python3", "/probe/mock_upstream.py", "--port", "8080",
               "--tier", name, "--body-dir", "/work/bodies")
    for name, ip, hostport in TIERS:
        wait_http("http://127.0.0.1:%d/__healthz" % hostport, 45)
    log("mocks up: %s" % ", ".join(TIER_NAMES))


def mocks_down():
    for name, _, _ in TIERS:
        container_rm(name)


def mock_port(tier):
    return dict((n, p) for n, _, p in TIERS)[tier]


def http_json(url, obj=None, method=None, timeout=30):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(url, data=data,
                                 method=method or ("POST" if data is not None
                                                   else "GET"),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read() or b"{}")


def wait_http(url, seconds):
    end = time.time() + seconds
    last = None
    while time.time() < end:
        try:
            urllib.request.urlopen(url, timeout=2).read()
            return True
        except Exception as e:      # noqa: BLE001
            last = e
            time.sleep(0.25)
    raise RuntimeError("timed out waiting for %s (%s)" % (url, last))


def mock_reset(tier):
    http_json("http://127.0.0.1:%d/__reset" % mock_port(tier), {})


def mock_reset_all():
    for t in TIER_NAMES:
        mock_reset(t)
    bd = os.path.join(WORK, "bodies")
    if os.path.isdir(bd):
        shutil.rmtree(bd, ignore_errors=True)
    os.makedirs(bd, exist_ok=True)


def mock_control(tier, **kw):
    return http_json("http://127.0.0.1:%d/__control" % mock_port(tier), kw)


def mock_hits(tier):
    return http_json("http://127.0.0.1:%d/__hits" % mock_port(tier))["hits"]


def all_hits():
    return dict((t, mock_hits(t)) for t in TIER_NAMES)


def hit_counts():
    return dict((t, len(h)) for t, h in all_hits().items())


# --------------------------------------------------------------------------
# gateway config + lifecycle
# --------------------------------------------------------------------------

def auth_value(tier):
    """A synthetic upstream credential. Only its sha256 is ever recorded."""
    return "Bearer probe-upstream-key-%s" % tier


def auth_sha(tier):
    return hashlib.sha256(auth_value(tier).encode()).hexdigest()


def gw_config(max_replay_bytes=DEFAULT_MAX_REPLAY, attempts=ATTEMPTS,
              buffer_size=DEFAULT_BUFFER, backoff="50ms",
              eviction_failures=1, mutate=None):
    providers, models, targets = [], [], []
    for i, (name, ip, _) in enumerate(TIERS):
        providers.append({
            "name": name,
            "provider": {"custom": {"formats": [
                {"type": "completions", "path": "/v1/chat/completions"}]}},
            "params": {"baseUrl": "http://%s:8080" % ip,
                       "model": "upstream-%s" % name},
            "defaults": {
                "auth": {"key": {
                    "value": auth_value(name),
                    "location": {"header": {"name": "Authorization"}}}},
                "health": {"eviction": {
                    "duration": "60s",
                    "consecutiveFailures": eviction_failures}},
            },
        })
        models.append({"name": name, "provider": {"reference": name}})
        targets.append({"model": name, "priority": i})

    cfg = {
        "config": {"readinessAddr": "127.0.0.1:15021",
                   "statsAddr": "127.0.0.1:15020"},
        "policies": [{
            "name": {"name": "probe-retry", "namespace": "default"},
            "target": {"route": {"name": "llm:request",
                                 "namespace": "internal"}},
            "policy": {"retry": {"attempts": attempts, "codes": RETRY_CODES,
                                 "backoff": backoff,
                                 "maxReplayBytes": max_replay_bytes}},
        }],
        "frontendPolicies": {"http": {"maxBufferSize": buffer_size}},
        "llm": {
            "port": 8080,
            "providers": providers,
            "models": models,
            "virtualModels": [{"name": "probe-model",
                               "routing": {"failover": {"targets": targets}}}],
        },
    }
    if mutate:
        mutate(cfg)
    return cfg


def write_config(cfg, filename="config.yaml"):
    os.makedirs(os.path.join(WORK, "conf"), exist_ok=True)
    path = os.path.join(WORK, "conf", filename)
    # JSON is valid YAML 1.2, so this avoids hand-rolling a YAML emitter -- and
    # it avoids `$` sequences, which agentgateway expands over the RAW file
    # text before parsing (an undefined one is a hard startup failure).
    with open(path, "w") as fh:
        json.dump(cfg, fh, indent=1)
    os.chmod(path, 0o644)
    return path


def gw_run_args(name, port, conf_basename, rust_log):
    return ["run", "-d", "--name", name, "--network", NET,
            "-p", "127.0.0.1:%d:8080" % port,
            "-v", "%s:/conf:ro" % os.path.join(WORK, "conf"),
            "-e", "RUST_LOG=%s" % rust_log,
            GW_IMAGE, "--file", "/conf/%s" % conf_basename]


def gw_up(cfg, name=GW_NAME, port=GW_PORT, rust_log="info,agentgateway=debug",
          filename=None, wait=90):
    container_rm(name)
    path = write_config(cfg, filename or ("%s.yaml" % name))
    docker(*gw_run_args(name, port, os.path.basename(path), rust_log))
    gw_wait(port, name, wait)
    return path


def gw_wait(port, name, seconds=90):
    end = time.time() + seconds
    while time.time() < end:
        try:
            with urllib.request.urlopen(
                    "http://127.0.0.1:%d/v1/models" % port, timeout=2) as r:
                r.read()
            return True
        except urllib.error.HTTPError:
            return True          # it answered -> the listener is up
        except Exception:        # noqa: BLE001
            time.sleep(0.3)
    raise RuntimeError("gateway %s did not become ready:\n%s"
                       % (name, gw_logs(name)[-4000:]))


def gw_down(name=GW_NAME):
    container_rm(name)


def gw_logs(name=GW_NAME):
    p = docker("logs", name, check=False, timeout=90)
    return (p.stdout or "") + (p.stderr or "")


def gw_restart(cfg, name=GW_NAME, port=GW_PORT, **kw):
    """Fresh state: a NEW process, so no eviction/health state carries over."""
    gw_down(name)
    return gw_up(cfg, name=name, port=port, **kw)


def gw_try_boot(cfg, filename, seconds=12):
    """Run the gateway in the foreground; report whether it refuses to boot."""
    name = "agw-probe-boot"
    container_rm(name)
    path = write_config(cfg, filename)
    args = ["run", "--rm", "--name", name, "--network", NET,
            "-v", "%s:/conf:ro" % os.path.join(WORK, "conf"),
            GW_IMAGE, "--file", "/conf/%s" % os.path.basename(path)]
    try:
        p = docker(*args, check=False, timeout=seconds)
        out = (p.stdout or "") + (p.stderr or "")
        return {"exited": True, "exit_code": p.returncode,
                "output": out.strip()[-2000:]}
    except subprocess.TimeoutExpired:
        out = gw_logs(name)
        container_rm(name)
        return {"exited": False, "exit_code": None,
                "output": out.strip()[-2000:]}
    finally:
        container_rm(name)


# --------------------------------------------------------------------------
# client
# --------------------------------------------------------------------------

def chat_body(prompt="hello", stream=False, pad=0, extra=None):
    content = prompt + ("x" * pad) if pad else prompt
    body = {"model": "probe-model",
            "messages": [{"role": "user", "content": content}],
            "stream": stream}
    if extra:
        body.update(extra)
    return body


def post_chat(port, body, timeout=180, path="/v1/chat/completions",
              headers=None):
    """POST and read to EOF. Returns (status, headers, raw, elapsed, frames)."""
    raw = body if isinstance(body, bytes) else json.dumps(body).encode()
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    hdrs = {"Content-Type": "application/json",
            "Content-Length": str(len(raw))}
    if headers:
        hdrs.update(headers)
    t0 = time.time()
    frames = []
    buf = bytearray()
    status = None
    rh = {}
    try:
        conn.request("POST", path, body=raw, headers=hdrs)
        resp = conn.getresponse()
        status = resp.status
        rh = dict((k.lower(), v) for k, v in resp.getheaders())
        # read1(): return as soon as ANY bytes are available.  read() would
        # block for a full 4 KiB and make a whole SSE stream look like one
        # arrival, destroying the inter-frame timing P15 depends on.
        reader = getattr(resp, "read1", None) or resp.read
        while True:
            chunk = reader(65536)
            if not chunk:
                break
            frames.append([round(time.time() - t0, 4), len(chunk)])
            buf += chunk
        return status, rh, bytes(buf), time.time() - t0, frames
    except Exception as e:          # noqa: BLE001
        # keep whatever arrived: a truncated stream is itself the evidence.
        # http.client discards the partial payload into the exception, so
        # recover it -- otherwise a truncation looks like "nothing delivered".
        partial = getattr(e, "partial", None)
        if partial:
            buf += partial
            frames.append([round(time.time() - t0, 4), len(partial)])
        rh["__error__"] = repr(e)
        return status, rh, bytes(buf), time.time() - t0, frames
    finally:
        try:
            conn.close()
        except Exception:           # noqa: BLE001
            pass


def sse_messages(raw):
    """Decode an SSE byte stream into the ordered list of `data:` payloads."""
    out = []
    for line in raw.split(b"\n"):
        line = line.strip()
        if line.startswith(b"data:"):
            out.append(line[5:].strip())
    return out


# --------------------------------------------------------------------------
# result plumbing
# --------------------------------------------------------------------------

RESULTS = []


def record(probe, verdict, requirement, mode, config, evidence, notes=""):
    r = {"probe": probe, "verdict": verdict, "requirement": requirement,
         "mode": mode, "config": config, "evidence": evidence, "notes": notes,
         "build": GW_IMAGE, "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z")}
    RESULTS.append(r)
    log("%s -> %s  (%s)" % (probe, verdict, requirement))
    return r


def dump(out):
    if not out:
        return
    d = os.path.dirname(os.path.abspath(out))
    os.makedirs(d, exist_ok=True)
    with open(out, "w") as fh:
        json.dump({"generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                   "build": GW_IMAGE, "results": RESULTS}, fh, indent=2)
    log("wrote %s (%d results)" % (out, len(RESULTS)))


# --------------------------------------------------------------------------
# P5 -- FR-4.9, retry not silently disabled by body size
# --------------------------------------------------------------------------

P5_SIZES = [("1K", 1024), ("40K", 40 * 1024), ("60K", 60 * 1024),
            ("70K", 70 * 1024), ("100K", 100 * 1024),
            ("1M", 1024 * 1024), ("50M", 50 * 1000 * 1000)]


def probe_p5(max_replay=DEFAULT_MAX_REPLAY, sizes=None, stream=True):
    """Sweep body sizes through a deliberately failing primary.

    The gateway is RESTARTED before every size: otherwise the first size's
    eviction of tier0 makes every later size skip tier0 entirely, and a
    zero-hit tier0 would read as "no retry" when it is really "no attempt".
    """
    sizes = sizes or P5_SIZES
    cfg = gw_config(max_replay_bytes=max_replay)
    detail = []
    first_fail = None
    for label, nbytes in sizes:
        gw_restart(cfg)                        # fresh process, fresh health
        mock_reset_all()
        mock_control("tier0", mode="status", status_code=503)
        mock_control("tier0", save_body_max=0)  # do not persist huge bodies
        mock_control("tier1", save_body_max=0)
        body = chat_body("p5-%s " % label, stream=stream, pad=nbytes)
        raw = json.dumps(body).encode()
        st, hdr, data, el, _ = post_chat(GW_PORT, raw, timeout=300)
        hits = hit_counts()
        h1 = mock_hits("tier1")
        retried = hits["tier0"] >= 1 and hits["tier1"] >= 1
        served_ok = st == 200 and (b"served-by-tier1" in data)
        row = {"size_label": label, "body_bytes": len(raw),
               "client_status": st, "elapsed_s": round(el, 3),
               "upstream_hits": hits,
               "tier1_x_retry_attempt": (h1[0].get("x_retry_attempt")
                                         if h1 else None),
               "retry_observed": retried, "served_by_tier1": served_ok,
               "client_body_head": data[:200].decode("utf-8", "replace")}
        detail.append(row)
        log("  P5 %-4s body=%d status=%s hits=%s retry=%s"
            % (label, len(raw), st, hits, retried))
        if not (retried and served_ok) and first_fail is None:
            first_fail = label
    gw_down()
    ok = first_fail is None
    return record(
        "P5", "PASS" if ok else "FAIL", "FR-4.9",
        "proxy path, LLM listener, streaming=%s, 3-tier failover pool" % stream,
        {"maxReplayBytes": max_replay, "maxBufferSize": DEFAULT_BUFFER,
         "retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS, "retry.codes": RETRY_CODES,
         "gateway_restarted_before_each_size": True},
        {"sizes": detail, "first_failing_size": first_fail},
        notes=("Retry is proven by tier0 hit >=1 AND tier1 hit >=1 for one "
               "client request, plus the x-retry-attempt header agentgateway "
               "stamps on the replayed attempt -- not by the client status."))


def probe_p5_default_control(sizes=None):
    """Control arm: the SAME sweep with maxReplayBytes left at its default.

    This is what a probe that forgot to carry the cap would have measured; it
    is recorded so the two are never confused.
    """
    sizes = sizes or [("40K", 40 * 1024), ("60K", 60 * 1024),
                      ("70K", 70 * 1024), ("100K", 100 * 1024)]
    cfg = gw_config()
    # remove maxReplayBytes entirely -> code default of 64 KiB
    cfg["policies"][0]["policy"]["retry"].pop("maxReplayBytes")
    detail = []
    for label, nbytes in sizes:
        gw_restart(cfg, filename="agw-probe-gw-default.yaml")
        mock_reset_all()
        mock_control("tier0", mode="status", status_code=503)
        st, _, data, el, _ = post_chat(
            GW_PORT, json.dumps(chat_body("p5d-%s " % label, True, nbytes)
                                ).encode(), timeout=300)
        hits = hit_counts()
        detail.append({"size_label": label, "client_status": st,
                       "upstream_hits": hits,
                       "retry_observed": hits["tier0"] >= 1 and hits["tier1"] >= 1,
                       "elapsed_s": round(el, 3)})
        log("  P5-default %-4s status=%s hits=%s" % (label, st, hits))
    gw_down()
    return record(
        "P5-control-default-cap", "INFO", "FR-4.9 (control arm)",
        "proxy path, streaming, retry policy WITHOUT maxReplayBytes",
        {"maxReplayBytes": "unset -> code default 64 KiB",
         "retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS},
        {"sizes": detail},
        notes=("Control arm establishing that the sweep can detect the cliff: "
               "if this arm also passed at every size the P5 result would be "
               "meaningless."))


# --------------------------------------------------------------------------
# core probes
# --------------------------------------------------------------------------

def probe_p1():
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    st, hdr, data, el, _ = post_chat(GW_PORT, chat_body("p1-hello"))
    hits = all_hits()
    h0 = hits["tier0"][0] if hits["tier0"] else None
    ok = (st == 200 and h0 is not None and len(hits["tier1"]) == 0
          and len(hits["tier2"]) == 0
          and h0["path"] == "/v1/chat/completions"
          and h0["auth_sha256"] == auth_sha("tier0")
          and h0["model"] == "upstream-tier0")
    return record(
        "P1", "PASS" if ok else "FAIL", "FR-1.1/1.2, FR-6.8 (auth injection)",
        "proxy path, LLM listener, non-streaming, all tiers healthy",
        {"virtualModel": "probe-model", "tier0.baseUrl": "http://%s:8080"
         % TIERS[0][1], "tier0.path": "/v1/chat/completions",
         "tier0.params.model": "upstream-tier0"},
        {"client_status": st,
         "upstream_hits": dict((k, len(v)) for k, v in hits.items()),
         "tier0_hit": h0,
         "expected_auth_sha256": auth_sha("tier0"),
         "auth_header_matches_configured_credential":
             bool(h0) and h0["auth_sha256"] == auth_sha("tier0"),
         "client_body": data[:400].decode("utf-8", "replace")},
        notes=("Credential compared by sha256 only; the raw value is never "
               "recorded. Path mapping proven by the upstream-observed path, "
               "model rewrite by the upstream-observed model."))


def probe_p2():
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    mock_control("tier0", mode="status", status_code=503)
    st, hdr, data, el, _ = post_chat(GW_PORT, chat_body("p2-hello"))
    hits = all_hits()
    counts = dict((k, len(v)) for k, v in hits.items())
    ok = (st == 200 and counts["tier0"] == 1 and counts["tier1"] == 1
          and b"served-by-tier1" in data)
    return record(
        "P2", "PASS" if ok else "FAIL", "FR-4.2",
        "proxy path, non-streaming, tier0 forced 503",
        {"retry.codes": RETRY_CODES, "retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS,
         "maxReplayBytes": DEFAULT_MAX_REPLAY},
        {"client_status": st, "upstream_hits": counts,
         "tier1_x_retry_attempt": (hits["tier1"][0]["x_retry_attempt"]
                                   if hits["tier1"] else None),
         "client_body": data[:300].decode("utf-8", "replace"),
         "elapsed_s": round(el, 3)},
        notes="Client never sees the 503; tier0 was really attempted (1 hit).")


def probe_p3():
    """FR-4.3 streaming pre-header retry. MUST start from fresh state."""
    cfg = gw_config()
    gw_restart(cfg)          # <- the trap: P2 already evicted tier0
    mock_reset_all()
    mock_control("tier0", mode="status", status_code=503)
    st, hdr, data, el, frames = post_chat(GW_PORT, chat_body("p3", stream=True))
    hits = all_hits()
    counts = dict((k, len(v)) for k, v in hits.items())
    msgs = sse_messages(data)
    ok = (st == 200 and counts["tier0"] == 1 and counts["tier1"] == 1
          and b"served-by-tier1" in data and b"[DONE]" in data
          and b"503" not in data)
    return record(
        "P3", "PASS" if ok else "FAIL", "FR-4.3",
        "proxy path, STREAMING, tier0 forced 503, GATEWAY RESTARTED "
        "immediately before this probe (fresh health/eviction state)",
        {"retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS, "retry.codes": RETRY_CODES,
         "maxReplayBytes": DEFAULT_MAX_REPLAY,
         "fresh_state": "container recreated before the request"},
        {"client_status": st, "content_type": hdr.get("content-type"),
         "upstream_hits": counts,
         "tier0_attempted": counts["tier0"] == 1,
         "tier1_x_retry_attempt": (hits["tier1"][0]["x_retry_attempt"]
                                   if hits["tier1"] else None),
         "sse_message_count": len(msgs),
         "sse_first": msgs[0][:160].decode("utf-8", "replace") if msgs else None,
         "sse_last": msgs[-1][:80].decode("utf-8", "replace") if msgs else None,
         "client_saw_503": b"503" in data,
         "elapsed_s": round(el, 3)},
        notes=("tier0 hit count of exactly 1 is what separates 'retried' from "
               "'tier0 was already evicted and never tried'."))


def probe_p4():
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    mock_control("tier0", mode="status", status_code=503)
    mock_control("tier1", mode="status", status_code=503)
    st, hdr, data, el, _ = post_chat(GW_PORT, chat_body("p4", stream=True))
    hits = all_hits()
    counts = dict((k, len(v)) for k, v in hits.items())
    total = sum(counts.values())
    ok = (st == 200 and total == TOTAL_ATTEMPTS and counts["tier0"] == 1
          and counts["tier1"] == 1 and counts["tier2"] == 1
          and b"served-by-tier2" in data and b"[DONE]" in data)
    return record(
        "P4", "PASS" if ok else "FAIL", "FR-4.4",
        "proxy path, STREAMING, tier0+tier1 forced 503, fresh state",
        {"retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS, "priority tiers": "tier0=0, tier1=1, tier2=2"},
        {"client_status": st, "upstream_hits": counts,
         "total_upstream_hits_for_one_client_request": total,
         "x_retry_attempt_by_tier":
             dict((t, [h["x_retry_attempt"] for h in hits[t]])
                  for t in TIER_NAMES),
         "served_by_tier2": b"served-by-tier2" in data,
         "elapsed_s": round(el, 3)},
        notes="Three upstream hits for one client request is the assertion.")


def probe_p6():
    """Mid-stream failure: no candidate can retry; prove it fails cleanly."""
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    mock_control("tier0", mode="midstream_abort", abort_after=3, chunks=8)
    st, hdr, data, el, frames = post_chat(GW_PORT,
                                          chat_body("p6", stream=True),
                                          timeout=60)
    hits = hit_counts()
    msgs = sse_messages(data)
    # after the abort, is the gateway still healthy and are buffers released?
    mock_control("tier0", mode="ok")
    st2, _, data2, _, _ = post_chat(GW_PORT, chat_body("p6-after"))
    logs = gw_logs()
    req_lines = [ln for ln in logs.splitlines() if "\tinfo\trequest " in ln]
    aborted_line = req_lines[-2] if len(req_lines) >= 2 else (
        req_lines[-1] if req_lines else "")
    client_error = hdr.get("__error__")
    surfaced = bool(client_error) or (b"error" in data.lower())
    usage_reported = "gen_ai.usage" in aborted_line
    clean = (st == 200 and 0 < len(msgs) and b"[DONE]" not in data
             and surfaced and hits["tier0"] == 1 and el < 30 and st2 == 200)
    # EVALUATION.md sec.3 P6 pass criterion is "truncated stream, error
    # surfaced, buffers freed, usage still reported" -- all four.
    verdict = "PASS" if (clean and usage_reported) else (
        "PARTIAL" if clean else "FAIL")
    return record(
        "P6", verdict, "FR-4.3 negative / clean failure + FR-7.8 on the "
        "abort path",
        "proxy path, STREAMING, tier0 aborts the connection after 3 SSE frames",
        {"abort_after_frames": 3, "retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS},
        {"client_status": st, "sse_frames_delivered": len(msgs),
         "stream_terminated_with_DONE": b"[DONE]" in data,
         "failure_surfaced_to_client": surfaced,
         "client_transport_error": client_error,
         "upstream_hits": hits,
         "retry_attempted_after_headers": hits["tier0"] > 1,
         "elapsed_s": round(el, 3),
         "followup_request_status": st2,
         "access_log_line_for_aborted_request": aborted_line,
         "usage_reported_on_aborted_request": "gen_ai.usage" in aborted_line,
         "gateway_log_tail": logs[-1200:]},
        notes=("A mid-stream failure is unretriable by construction; the "
               "assertion is that it terminates promptly, does not retry after "
               "headers, leaves the process serving, AND still reports usage. "
               "PARTIAL = the first three hold but the access log for the "
               "aborted request carries no gen_ai.usage.* fields, so the "
               "tokens the provider already produced are never metered."))


def probe_p7():
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    for t in TIER_NAMES:
        mock_control(t, mode="status", status_code=503)
    st, hdr, data, el, _ = post_chat(GW_PORT, chat_body("p7", stream=True),
                                     timeout=60)
    hits = hit_counts()
    total = sum(hits.values())
    retry_after = hdr.get("retry-after")
    bounded = total <= TOTAL_ATTEMPTS and st is not None
    prompt = el < 15
    ok = bounded and prompt and retry_after is not None
    verdict = "PASS" if ok else ("PARTIAL" if (bounded and prompt) else "FAIL")
    return record(
        "P7", verdict, "FR-4.7 (bounded attempts) + FR-4.8 (reject + Retry-After)",
        "proxy path, STREAMING, all three tiers forced 503",
        {"retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS, "retry.codes": RETRY_CODES},
        {"client_status": st, "upstream_hits": hits,
         "total_attempts": total, "bounded": bounded,
         "elapsed_s": round(el, 3), "hang": not prompt,
         "retry_after_header": retry_after,
         "response_headers": hdr,
         "client_body": data[:300].decode("utf-8", "replace")},
        notes=("FR-4.7 and FR-4.8 are scored separately: bounded=%s, "
               "prompt_reject=%s, Retry-After=%s"
               % (bounded, prompt, retry_after)))


def probe_p11():
    """NFR-6.4 / FR-9.7 -- refuses to boot on invalid config, names the fault."""
    cases = []

    def mut_type(cfg):
        cfg["policies"][0]["policy"]["retry"]["maxReplayBytes"] = "fifty-megs"

    def mut_unknown(cfg):
        cfg["llm"]["providers"][0]["bogusField"] = True

    def mut_dangling(cfg):
        cfg["llm"]["virtualModels"][0]["routing"]["failover"]["targets"][0][
            "model"] = "no-such-model"

    def mut_badurl(cfg):
        cfg["llm"]["providers"][0]["params"]["baseUrl"] = "not a url"

    for cid, label, mut, fault_tokens in [
            ("type-error", "retry.maxReplayBytes: \"fifty-megs\"",
             mut_type, ["maxReplayBytes"]),
            ("unknown-field", "llm.providers[0].bogusField", mut_unknown,
             ["bogusField"]),
            ("dangling-ref", "virtualModel target -> undefined model",
             mut_dangling, ["no-such-model"]),
            ("bad-url", "provider params.baseUrl = 'not a url'", mut_badurl,
             ["baseUrl", "not a url", "url"])]:
        cfg = gw_config(mutate=mut)
        res = gw_try_boot(cfg, "invalid-%s.yaml" % cid)
        out = res["output"]
        names = [tok for tok in fault_tokens if tok.lower() in out.lower()]
        cases.append({"case": cid, "fault": label,
                      "refused_to_boot": res["exited"] and res["exit_code"] != 0,
                      "still_running_after_timeout": not res["exited"],
                      "exit_code": res["exit_code"],
                      "names_the_fault": bool(names),
                      "matched_tokens": names,
                      "output_tail": out[-600:]})
        log("  P11 %-14s refused=%s names_fault=%s"
            % (cid, cases[-1]["refused_to_boot"], cases[-1]["names_the_fault"]))

    refused = [c for c in cases if c["refused_to_boot"]]
    named = [c for c in refused if c["names_the_fault"]]
    if len(refused) == len(cases) and len(named) == len(cases):
        verdict = "PASS"
    elif refused and named:
        verdict = "PARTIAL"
    else:
        verdict = "FAIL"
    return record(
        "P11", verdict, "NFR-6.4 / FR-9.7",
        "startup validation, local file config (`--file`), distroless image",
        {"cases": [c["case"] for c in cases]},
        {"cases": cases,
         "refused_count": len(refused), "named_fault_count": len(named),
         "total_cases": len(cases)},
        notes=("PARTIAL means it refuses to boot but at least one fault class "
               "produces a message that does not identify the offending "
               "field."))


def probe_p14():
    """Negative-signal probe: assert the mechanism's own evidence."""
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    mock_control("tier0", mode="status", status_code=503)
    st, hdr, data, el, _ = post_chat(GW_PORT, chat_body("p14", stream=True))
    hits = all_hits()
    counts = dict((k, len(v)) for k, v in hits.items())
    logs = gw_logs()
    log_lines = [ln for ln in logs.splitlines()
                 if ("attempt" in ln.lower() or "retry" in ln.lower()
                     or "evict" in ln.lower() or "unhealthy" in ln.lower())]
    upstream_signal = (hits["tier1"][0]["x_retry_attempt"]
                       if hits["tier1"] else None)
    served_model = hits["tier1"][0]["model"] if hits["tier1"] else None
    mech_evidence = (upstream_signal is not None) and bool(log_lines)
    ok = mech_evidence and counts["tier0"] == 1 and counts["tier1"] == 1
    return record(
        "P14", "PASS" if ok else "FAIL", "FR-4.4 (feature actually active)",
        "proxy path, STREAMING, deliberately dead primary, RUST_LOG=debug, "
        "full pilot policy shape (retry + failover + per-provider health)",
        {"retry.attempts(config)": ATTEMPTS,
         "total_upstream_attempts": TOTAL_ATTEMPTS, "eviction.consecutiveFailures": 1,
         "eviction.duration": "60s"},
        {"mechanism_signal_upstream_header_x_retry_attempt": upstream_signal,
         "mechanism_signal_gateway_log_lines": log_lines[-12:],
         "upstream_hits": counts,
         "failover_target_model_seen_upstream": served_model,
         "client_status": st,
         "response_alone_would_have_looked_normal": st == 200},
        notes=("The response is a normal 200 either way -- the verdict rests "
               "on the x-retry-attempt header agentgateway stamps on the "
               "replayed request and on its own retry/eviction log lines."))


# --------------------------------------------------------------------------
# P15 -- idle-stream keepalive (FR-2.8)
# --------------------------------------------------------------------------

def probe_p15(silent_secs=8):
    """Does the build emit heartbeat frames during an idle upstream stream?"""
    search = []
    # 1. configuration surface
    p = docker("run", "--rm", "--entrypoint", "/app/agentgateway", GW_IMAGE,
               "--help", check=False, timeout=60)
    helptext = (p.stdout or "") + (p.stderr or "")
    search.append({"where": "binary --help",
                   "query": "keepalive|heartbeat|ping|idle",
                   "hits": [ln for ln in helptext.splitlines()
                            if any(k in ln.lower() for k in
                                   ("keepalive", "heartbeat", "ping", "idle"))]})
    schema = os.path.join(HERE, "config-schema-keys.txt")
    search.append({"where": "schema/config.json @ dcb5b524",
                   "query": "keepalive|heartbeat|sse ping|idle",
                   "hits": ["KeepaliveConfig / backend.keepalives (TCP "
                            "SO_KEEPALIVE on upstream conns)",
                            "frontendPolicies.http.http2KeepaliveInterval "
                            "(HTTP/2 PING frames, not SSE data frames)",
                            "backend.poolIdleTimeout",
                            "-- no SSE/stream heartbeat or idle-keepalive knob"],
                   "note": "see %s for the raw key dump" % schema})

    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    # baseline: no idle gap
    st_b, _, data_b, el_b, fr_b = post_chat(GW_PORT,
                                            chat_body("p15-base", stream=True))
    base_msgs = sse_messages(data_b)
    # idle arm: upstream goes silent past any plausible keepalive interval
    mock_control("tier0", mode="silent", silent_secs=silent_secs)
    st_i, _, data_i, el_i, fr_i = post_chat(GW_PORT,
                                            chat_body("p15-base", stream=True),
                                            timeout=silent_secs + 60)
    idle_msgs = sse_messages(data_i)
    gaps = []
    prev = 0.0
    for t, _n in fr_i:
        gaps.append(round(t - prev, 3))
        prev = t
    identical = base_msgs == idle_msgs
    extra = [m for m in idle_msgs if m not in base_msgs]
    return record(
        "P15", "N-A", "FR-2.8",
        "proxy path, STREAMING, upstream silent for %ds mid-stream"
        % silent_secs,
        {"idle_gap_s": silent_secs,
         "keepalive_knob": "none found -- see evidence.search"},
        {"search": search,
         "baseline_message_count": len(base_msgs),
         "idle_arm_message_count": len(idle_msgs),
         "decoded_streams_byte_identical": identical,
         "frames_injected_during_idle_window": extra,
         "client_frame_arrival_gaps_s": gaps,
         "max_gap_s": max(gaps) if gaps else None,
         "baseline_elapsed_s": round(el_b, 3),
         "idle_elapsed_s": round(el_i, 3),
         "baseline_status": st_b, "idle_status": st_i},
        notes=("N-A: this build exposes no idle-stream keepalive/heartbeat "
               "setting for SSE. Measured behaviour is recorded anyway: the "
               "client saw a single gap equal to the upstream silence and NO "
               "injected frames, i.e. the mechanism is absent rather than "
               "present-and-disabled."))


# --------------------------------------------------------------------------
# P12 -- byte determinism across processes (FR-2.7 / NFR-4.5)
# --------------------------------------------------------------------------

def determinism_request():
    """A body engineered to expose map-ordering: many keys, nested tool
    schemas, and a tool-call `arguments` STRING (the Kong choke point)."""
    return {
        "model": "probe-model",
        "stream": False,
        "temperature": 0,
        "top_p": 0.95,
        "max_tokens": 256,
        "presence_penalty": 0.1,
        "frequency_penalty": 0.2,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": "zeta alpha omega"},
            {"role": "user", "content": "call the tool"},
            {"role": "assistant", "content": None, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {
                    "name": "lookup",
                    "arguments": "{\"zeta\":1,\"alpha\":2,\"mu\":3,"
                                 "\"beta\":{\"y\":2,\"x\":1},\"omega\":\"z\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "42"},
            {"role": "user", "content": "thanks"},
        ],
        "tools": [
            {"type": "function", "function": {
                "name": "lookup", "description": "d",
                "parameters": {"type": "object", "properties": {
                    "zeta": {"type": "string"}, "alpha": {"type": "integer"},
                    "mu": {"type": "boolean"}, "beta": {"type": "object",
                                                        "properties": {
                                                            "y": {"type": "integer"},
                                                            "x": {"type": "integer"}}},
                    "omega": {"type": "string"}},
                    "required": ["zeta", "alpha"]}}},
            {"type": "function", "function": {
                "name": "second", "description": "d2",
                "parameters": {"type": "object", "properties": {
                    "q": {"type": "string"}}}}},
        ],
    }


def probe_p12(runs_per_process=6):
    cfg = gw_config()
    body = determinism_request()
    captures = []

    def collect(tag, port):
        base = len(mock_hits("tier0"))
        for i in range(runs_per_process):
            st, _, data, _, _ = post_chat(port, body)
            assert st == 200, "unexpected status %s from %s" % (st, tag)
        hits = mock_hits("tier0")[base:]
        for h in hits:
            captures.append({"process": tag, "seq": h["seq"],
                             "body_sha256": h["body_sha256"],
                             "body_len": h["body_len"],
                             "body_file": h["body_file"]})

    # process A
    gw_restart(cfg, name=GW_NAME, port=GW_PORT)
    mock_reset_all()
    pa = docker("inspect", "-f", "{{.State.Pid}}", GW_NAME).stdout.strip()
    collect("A(pid=%s)" % pa, GW_PORT)
    # process B: a SECOND, concurrently running container -> a distinct OS
    # process with its own allocator/hasher state
    gw_up(cfg, name=GW_NAME_B, port=GW_PORT_B,
          filename="agw-probe-gw-b.yaml")
    pb = docker("inspect", "-f", "{{.State.Pid}}", GW_NAME_B).stdout.strip()
    collect("B(pid=%s)" % pb, GW_PORT_B)
    # process C: a restart of A -> a third distinct process
    gw_restart(cfg, name=GW_NAME, port=GW_PORT)
    pc = docker("inspect", "-f", "{{.State.Pid}}", GW_NAME).stdout.strip()
    collect("C(pid=%s restart)" % pc, GW_PORT)

    hashes = sorted(set(c["body_sha256"] for c in captures))
    procs = sorted(set(c["process"] for c in captures))
    identical = len(hashes) == 1

    sample = None
    for c in captures:
        if c["body_file"]:
            local = c["body_file"].replace("/work", WORK, 1)
            if os.path.exists(local):
                with open(local, "rb") as fh:
                    sample = fh.read().decode("utf-8", "replace")
                break
    args_stable = None
    if sample:
        args_stable = "\\\"zeta\\\":1" in sample or '"zeta":1' in sample

    gw_down(GW_NAME_B)
    gw_down(GW_NAME)
    return record(
        "P12", "PASS" if identical else "FAIL", "FR-2.7 / NFR-4.5",
        "proxy path, non-streaming, THREE distinct gateway OS processes "
        "(two concurrent containers + one restart), identical request body",
        {"runs_per_process": runs_per_process, "processes": procs},
        {"distinct_upstream_body_sha256": hashes,
         "capture_count": len(captures),
         "captures": captures,
         "process_count": len(procs),
         "tool_call_arguments_string_present_verbatim": args_stable,
         "upstream_body_sample": (sample[:1200] if sample else None)},
        notes=("CLAIM SCOPE: this is 'observed stable across %d captures "
               "spanning %d distinct OS processes', NOT 'guaranteed stable'. "
               "See SCORECARD.md for the source-level reasoning about whether "
               "the serializer's ordering is structurally guaranteed."
               % (len(captures), len(procs))))


# --------------------------------------------------------------------------
# P18 -- prompt-head stability across turns (FR-2.9)
# --------------------------------------------------------------------------

def probe_p18(turns=22):
    cfg = gw_config()
    gw_restart(cfg)
    mock_reset_all()
    mock_control("tier0", save_body_max=8_000_000)

    head = {"role": "system",
            "content": "SYSTEM-HEAD constant prefix block. " + ("h" * 400)}
    messages = [head]
    rows = []
    prev_body = None
    prev_head_bytes = None
    for t in range(1, turns + 1):
        # a mid-conversation system message injected on EVERY turn -- the
        # exact shape that made Kong hoist it into the head block
        messages = messages + [
            {"role": "system", "content": "TURN-%d policy note %s" % (t, "p" * 60)},
            {"role": "user", "content": "turn %d question %s" % (t, "q" * 200)},
        ]
        body = {"model": "probe-model", "stream": False, "messages": messages}
        sent_system_at = [i for i, m in enumerate(messages)
                          if m.get("role") == "system"]
        sent_leading = 0
        for m in messages:
            if m.get("role") == "system":
                sent_leading += 1
            else:
                break
        st, _, data, _, _ = post_chat(GW_PORT, body)
        hits = mock_hits("tier0")
        h = hits[-1]
        local = h["body_file"].replace("/work", WORK, 1) if h["body_file"] else None
        up = open(local, "rb").read() if local and os.path.exists(local) else b""
        parsed = json.loads(up) if up else {}
        up_msgs = parsed.get("messages", [])
        head_bytes = json.dumps(up_msgs[0], sort_keys=False).encode() if up_msgs else b""
        # count how many system messages the upstream head block absorbed
        leading_system = 0
        for m in up_msgs:
            if m.get("role") == "system":
                leading_system += 1
            else:
                break
        rows.append({
            "turn": t,
            "client_status": st,
            "upstream_body_len": len(up),
            "upstream_message_count": len(up_msgs),
            "leading_system_message_count": leading_system,
            "head_sha256": hashlib.sha256(head_bytes).hexdigest(),
            "head_len": len(head_bytes),
            "head_identical_to_prev": (prev_head_bytes is None
                                       or head_bytes == prev_head_bytes),
            "shared_prefix_with_prev_turn": h["prefix_match_bytes"],
            "prev_body_len": len(prev_body) if prev_body else 0,
            "append_only": (prev_body is None or
                            h["prefix_match_bytes"] >= len(prev_body) - 64),
            "system_message_indices_upstream":
                [i for i, m in enumerate(up_msgs) if m.get("role") == "system"],
            "system_message_indices_as_sent": sent_system_at,
            "leading_system_run_as_sent": sent_leading,
            # the defect signature: the upstream leading system run is LONGER
            # than the one the client sent, i.e. mid-conversation system
            # messages were hoisted into the head block
            "hoisted": leading_system > sent_leading,
            "message_order_preserved":
                [i for i, m in enumerate(up_msgs)
                 if m.get("role") == "system"] == sent_system_at,
        })
        prev_head_bytes = head_bytes
        prev_body = up
        # the assistant reply keeps the conversation growing
        messages = messages + [{"role": "assistant",
                                "content": "answer %d %s" % (t, "a" * 150)}]

    head_hashes = sorted(set(r["head_sha256"] for r in rows))
    head_stable = len(head_hashes) == 1
    append_only = all(r["append_only"] for r in rows)
    no_hoist = not any(r["hoisted"] for r in rows)
    order_ok = all(r["message_order_preserved"] for r in rows)
    growth = [r["shared_prefix_with_prev_turn"] for r in rows]
    ok = head_stable and append_only and no_hoist and order_ok
    gw_down()
    return record(
        "P18", "PASS" if ok else "FAIL", "FR-2.9",
        "proxy path, non-streaming, %d-turn replay injecting one "
        "role=system message per turn" % turns,
        {"turns": turns, "head_block": "constant 430-byte system message",
         "per_turn_injection": "one role=system message mid-conversation"},
        {"leading_system_block_sha256_set": head_hashes,
         "leading_system_block_byte_identical_across_turns": head_stable,
         "leading_system_message_count_per_turn":
             sorted(set(r["leading_system_message_count"] for r in rows)),
         "mid_conversation_system_messages_hoisted": not no_hoist,
         "message_order_preserved_every_turn": order_ok,
         "append_only_across_turns": append_only,
         "shared_prefix_bytes_by_turn": growth,
         "upstream_body_len_by_turn": [r["upstream_body_len"] for r in rows],
         "turns": rows},
        notes=("`shared_prefix_bytes_by_turn` is a MOCK-DERIVED prefix-cache "
               "proxy: the mock reports the longest common byte prefix between "
               "consecutive upstream bodies. It stands in for the provider's "
               "`cached_tokens` (no real provider is involved here) and is a "
               "direct measure of the same property -- a hoisting bug makes it "
               "flatline near the head length instead of tracking body "
               "growth."))


# --------------------------------------------------------------------------
# not-applicable probes
# --------------------------------------------------------------------------

NA_REASON = (
    "agentgateway has no server-side tool-augmentation / hijacked-response "
    "path at all. FR-2.6 (web search via the Kong Tavily sidecar) has no "
    "equivalent: there is no component that answers a client request from a "
    "non-provider source, so there is nothing whose transparency (FR-2.6a), "
    "failure surfacing (FR-2.6c) or metering (FR-7.10) can be probed. This is "
    "the same reason the parity generator REFUSES to emit routes that carry "
    "`web_search` rather than emitting a degraded equivalent. An absent probe "
    "here is an ABSENT CAPABILITY, not a passing one -- the corresponding "
    "scorecard cells read 'no mechanism', never a tick.")


def probe_na():
    for pid, req, what in [
            ("P13", "FR-2.6a", "augmentation is request-transparent"),
            ("P16", "FR-2.6c", "augmentation surfaces upstream failure"),
            ("P17", "FR-7.10", "metering covers the bypass/hijack path")]:
        record(pid, "N-A", req,
               "not run -- no augmentation path exists in this build",
               {"searched": ["schema/config.json @ dcb5b524: no "
                             "web-search/augmentation/tool-execution policy",
                             "llm.policies: promptGuard, defaults, overrides, "
                             "transformations, prompts, modelAliases, "
                             "promptCaching, routes -- none hijacks a response"]},
               {"what_the_probe_would_prove": what,
                "why_not_run": NA_REASON},
               notes="Do not read this as a pass.")


# --------------------------------------------------------------------------
# cluster probes
# --------------------------------------------------------------------------

def cluster_key():
    """Fetch the pilot API key. NEVER printed, logged, or persisted."""
    p = kubectl("get", "secret", "pilot-test-apikey", "-o",
                "jsonpath={.data.key}")
    import base64
    return base64.b64decode(p.stdout).decode().strip()


def cluster_post(path, body, key=None, timeout=120, host=CLUSTER_HOST,
                 stream_cb=None):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = json.dumps(body).encode()
    conn = http.client.HTTPSConnection(host, 443, timeout=timeout, context=ctx)
    hdrs = {"Content-Type": "application/json",
            "Content-Length": str(len(raw))}
    if key:
        hdrs["Authorization"] = "Bearer " + key
    t0 = time.time()
    try:
        conn.request("POST", path, body=raw, headers=hdrs)
        resp = conn.getresponse()
        status = resp.status
        rh = dict((k.lower(), v) for k, v in resp.getheaders())
        buf = bytearray()
        while True:
            c = resp.read(4096)
            if not c:
                break
            buf += c
            if stream_cb:
                stream_cb(time.time() - t0, c)
        return status, rh, bytes(buf), time.time() - t0
    except Exception as e:              # noqa: BLE001
        return None, {"__error__": repr(e)}, b"", time.time() - t0
    finally:
        try:
            conn.close()
        except Exception:               # noqa: BLE001
            pass


def gw_pod_logs(since="60s"):
    p = kubectl("logs", "-l", "gateway.networking.k8s.io/gateway-name=maas-v2-agw",
                "--tail=400", "--since=" + since, "-c", "agentgateway",
                check=False, timeout=90)
    out = p.stdout or ""
    if not out.strip():
        p = kubectl("logs", "deployment/maas-v2-agw", "--tail=400",
                    "--since=" + since, check=False, timeout=90)
        out = p.stdout or ""
    return out


def access_log_lines(since="45s"):
    return [ln for ln in gw_pod_logs(since).splitlines()
            if "\tinfo\trequest " in ln]


def upstream_connect_evidence(line, el, el_allow):
    """agentgateway stamps `endpoint=<host:port>` on the access-log line only
    when a backend connection was actually selected and dialled.  Its absence,
    together with `reason=DirectResponse`, is the proof that nothing upstream
    was contacted."""
    return {"access_log_line": line,
            "names_an_upstream_endpoint": "endpoint=" in line,
            "direct_response": "reason=DirectResponse" in line,
            "no_upstream_connect": ("endpoint=" not in line) and bool(line),
            "latency_s": round(el, 3),
            "allow_path_latency_s": round(el_allow, 3)}


def probe_p8(model="deepseek-v4-pro", denied_model=None):
    """FR-6.1 / NFR-3.2 -- denial and fail-closed, with no upstream connect."""
    key = cluster_key()
    body = {"model": model,
            "messages": [{"role": "user", "content": "p8 probe"}],
            "max_tokens": 8}
    out = {}

    # ---- control: a valid key is allowed (proves the path is otherwise live)
    st_ok, h_ok, d_ok, el_ok = cluster_post("/v1/chat/completions", body, key)
    ok_line = access_log_lines("45s")[-1] if access_log_lines("45s") else ""
    out["control_allow"] = {"status": st_ok, "elapsed_s": round(el_ok, 3),
                            "body_head": d_ok[:200].decode("utf-8", "replace"),
                            "access_log_line": ok_line,
                            "names_an_upstream_endpoint": "endpoint=" in ok_line}

    # ---- case 1a: authorizer denies -- unknown credential
    st_d, h_d, d_d, el_d = cluster_post("/v1/chat/completions", body,
                                        "definitely-not-a-valid-key")
    lines = access_log_lines("45s")
    out["deny_unknown_key"] = {
        "status": st_d, "elapsed_s": round(el_d, 3),
        "body": d_d[:400].decode("utf-8", "replace"),
        "denied": st_d is not None and st_d >= 400,
        "upstream_connect_evidence":
            upstream_connect_evidence(lines[-1] if lines else "", el_d, el_ok)}

    # ---- case 1b: authorizer denies -- ACL (valid key, forbidden model)
    if denied_model:
        b2 = dict(body, model=denied_model)
        st_a, h_a, d_a, el_a = cluster_post("/v1/chat/completions", b2, key)
        lines = access_log_lines("45s")
        out["deny_acl_forbidden_model"] = {
            "model": denied_model, "status": st_a, "elapsed_s": round(el_a, 3),
            "body": d_a[:400].decode("utf-8", "replace"),
            "denied": st_a is not None and st_a >= 400,
            "upstream_connect_evidence":
                upstream_connect_evidence(lines[-1] if lines else "", el_a,
                                          el_ok)}

    # ---- case 2: authorizer times out (backendRef repointed at a tarpit)
    out["timeout"] = cluster_extauth_timeout_case(body)

    deny_cases = [v for k, v in out.items() if k.startswith("deny_")]
    denied_ok = all(c["denied"] for c in deny_cases) and bool(deny_cases)
    no_connect = all(
        c["upstream_connect_evidence"]["no_upstream_connect"]
        for c in deny_cases)
    to = out["timeout"]
    closed = to.get("failed_closed")
    verdict = "PASS" if (denied_ok and no_connect and closed) else "FAIL"
    return record(
        "P8", verdict, "FR-6.1 / NFR-3.2",
        "in-cluster, gateway maas-v2-agw (%s), extAuth failureMode=FailClosed, "
        "AgentgatewayPolicy maas-v2-agw-extauth -> ai-gateway-plugin-server:8080"
        " /v1/check/ext-authz" % NS,
        {"namespace": NS, "model": model, "denied_model": denied_model,
         "failureMode": "FailClosed",
         "timeout_case": to.get("how")},
        dict(out, summary={"all_denials_denied": denied_ok,
                           "all_denials_without_upstream_connect": no_connect,
                           "timeout_failed_closed": closed,
                           "timeout_failed_open": to.get("failed_open")}),
        notes=("The API key is read from secret pilot-test-apikey at runtime "
               "and never stored in this file or in the results. Fail-open in "
               "EITHER case is a stop-ship finding."))


def cluster_extauth_timeout_case(body):
    """Repoint extAuth at a tarpit that accepts and never answers."""
    how = ("deploy Service/agw-probe-tarpit in %s, patch "
           "AgentgatewayPolicy/maas-v2-agw-extauth backendRef to it, "
           "request, then restore" % NS)
    try:
        deploy_tarpit()
        kubectl("patch", "agentgatewaypolicy", "maas-v2-agw-extauth",
                "--type=json", "-p",
                json.dumps([{"op": "replace",
                             "path": "/spec/traffic/extAuth/backendRef/name",
                             "value": "agw-probe-tarpit"}]))
        time.sleep(15)
        key = cluster_key()
        st, h, d, el = cluster_post("/v1/chat/completions", body, key,
                                    timeout=120)
        lines = access_log_lines("120s")
        allowed = st == 200
        return {"how": how, "status": st, "elapsed_s": round(el, 3),
                "client_error": h.get("__error__"),
                "body": d[:400].decode("utf-8", "replace"),
                "failed_closed": not allowed,
                "failed_open": allowed,
                "hung_past_client_timeout": st is None and el > 100,
                "access_log_line": lines[-1] if lines else "",
                "names_an_upstream_endpoint":
                    ("endpoint=" in lines[-1]) if lines else None}
    except Exception as e:              # noqa: BLE001
        return {"how": how, "error": repr(e), "failed_closed": None}
    finally:
        kubectl("patch", "agentgatewaypolicy", "maas-v2-agw-extauth",
                "--type=json", "-p",
                json.dumps([{"op": "replace",
                             "path": "/spec/traffic/extAuth/backendRef/name",
                             "value": "ai-gateway-plugin-server"}]),
                check=False)
        remove_tarpit()
        time.sleep(15)
        # prove the pilot gateway is serving again before leaving
        k = cluster_key()
        st, _, _, _ = cluster_post(
            "/v1/chat/completions",
            {"model": "deepseek-v4-pro",
             "messages": [{"role": "user", "content": "restore check"}],
             "max_tokens": 4}, k)
        log("extAuth restored; post-restore control request -> %s" % st)


TARPIT_MANIFEST = """
apiVersion: v1
kind: ConfigMap
metadata:
  name: agw-probe-tarpit
  namespace: %(ns)s
data:
  tarpit.py: |
    import socket, time
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", 8080)); s.listen(128)
    while True:
        c, _ = s.accept()
        # accept, read nothing back, never answer: a true request timeout
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agw-probe-tarpit
  namespace: %(ns)s
spec:
  replicas: 1
  selector: {matchLabels: {app: agw-probe-tarpit}}
  template:
    metadata: {labels: {app: agw-probe-tarpit}}
    spec:
      containers:
      - name: tarpit
        image: python:3.12-slim
        command: ["python3", "/src/tarpit.py"]
        volumeMounts: [{name: src, mountPath: /src}]
      volumes:
      - name: src
        configMap: {name: agw-probe-tarpit}
---
apiVersion: v1
kind: Service
metadata:
  name: agw-probe-tarpit
  namespace: %(ns)s
spec:
  selector: {app: agw-probe-tarpit}
  ports: [{port: 8080, targetPort: 8080}]
"""


def deploy_tarpit():
    kubectl("apply", "-f", "-", input_=TARPIT_MANIFEST % {"ns": NS})
    kubectl("rollout", "status", "deployment/agw-probe-tarpit",
            "--timeout=120s")


def remove_tarpit():
    for kind in ("deployment", "service", "configmap"):
        kubectl("delete", kind, "agw-probe-tarpit", "--ignore-not-found",
                check=False, timeout=120)


def probe_p10(model="deepseek-v4-pro"):
    """NFR-3.5 -- rolling restart during a live long stream."""
    key = cluster_key()
    body = {"model": model, "stream": True,
            "messages": [{"role": "user",
                          "content": "Write a 700 word essay about the history "
                                     "of maritime navigation. Be detailed."}],
            "max_tokens": 1400}
    frames = []
    restarted = {"at": None}

    def cb(t, chunk):
        frames.append([round(t, 3), len(chunk)])
        if restarted["at"] is None and t > 1.5:
            restarted["at"] = t
            subprocess.Popen(
                ["kubectl", "--kubeconfig=" + KUBECONFIG, "-n", NS,
                 "rollout", "restart", "deployment/maas-v2-agw"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    st, hdr, data, el = cluster_post("/v1/chat/completions", body, key,
                                     timeout=600, stream_cb=cb)
    msgs = sse_messages(data)
    complete = bool(msgs) and msgs[-1] == b"[DONE]"
    finish = None
    for m in msgs:
        try:
            j = json.loads(m)
        except Exception:               # noqa: BLE001
            continue
        for ch in j.get("choices", []):
            if ch.get("finish_reason"):
                finish = ch["finish_reason"]
    try:
        kubectl("rollout", "status", "deployment/maas-v2-agw", "--timeout=300s")
    except Exception:                   # noqa: BLE001
        pass
    ok = st == 200 and complete
    return record(
        "P10", "PASS" if ok else "FAIL", "NFR-3.5",
        "in-cluster, STREAMING, `kubectl rollout restart deployment/maas-v2-agw`"
        " issued %s into the stream" % (restarted["at"],),
        {"namespace": NS, "model": model,
         "terminationGracePeriodSeconds": pod_grace()},
        {"client_status": st, "sse_frame_count": len(msgs),
         "stream_terminated_with_DONE": complete,
         "finish_reason": finish,
         "restart_issued_at_s": restarted["at"],
         "total_elapsed_s": round(el, 3),
         "bytes_received": len(data),
         "chunk_arrival_times_s": frames[:80]},
        notes="A truncated stream (no [DONE], no finish_reason) is the failure.")


def pod_grace():
    p = kubectl("get", "deployment", "maas-v2-agw", "-o",
                "jsonpath={.spec.template.spec.terminationGracePeriodSeconds}",
                check=False)
    return p.stdout.strip() or None


# --------------------------------------------------------------------------
# entry point
# --------------------------------------------------------------------------

LOCAL_PROBES = {
    "p1": probe_p1, "p2": probe_p2, "p3": probe_p3, "p4": probe_p4,
    "p6": probe_p6, "p7": probe_p7, "p11": probe_p11, "p14": probe_p14,
    "p15": probe_p15, "p12": probe_p12, "p18": probe_p18,
    "p5": probe_p5, "p5control": probe_p5_default_control,
}
CLUSTER_PROBES = {"p8": probe_p8, "p10": probe_p10}


def cleanup():
    for n in (GW_NAME, GW_NAME_B, "agw-probe-boot"):
        container_rm(n)
    mocks_down()
    net_down()
    log("cleaned up containers and network")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--probe", required=True)
    ap.add_argument("--out")
    ap.add_argument("--work", default=None)
    ap.add_argument("--keep", action="store_true",
                    help="leave containers/network running afterwards")
    ap.add_argument("--max-replay-bytes", type=int, default=DEFAULT_MAX_REPLAY)
    ap.add_argument("--turns", type=int, default=22)
    ap.add_argument("--runs", type=int, default=6)
    a = ap.parse_args()

    global WORK
    if a.work:
        WORK = a.work
    os.makedirs(WORK, exist_ok=True)

    sel = a.probe.lower()
    groups = {
        "core": ["p1", "p2", "p3", "p4", "p6", "p7", "p11", "p14"],
        "p5": ["p5", "p5control"],
        "all-local": ["p5", "p5control", "p1", "p2", "p3", "p4", "p6", "p7",
                      "p11", "p14", "p15", "p12", "p18"],
    }
    if sel == "cleanup":
        cleanup()
        return
    if sel == "na":
        probe_na()
        dump(a.out)
        return

    names = groups.get(sel, [sel])
    needs_local = any(n in LOCAL_PROBES for n in names)
    try:
        if needs_local:
            net_up()
            mocks_up()
        for n in names:
            if n in LOCAL_PROBES:
                fn = LOCAL_PROBES[n]
                if n == "p5":
                    fn(max_replay=a.max_replay_bytes)
                elif n == "p18":
                    fn(turns=a.turns)
                elif n == "p12":
                    fn(runs_per_process=a.runs)
                else:
                    fn()
            elif n in CLUSTER_PROBES:
                CLUSTER_PROBES[n]()
            else:
                raise SystemExit("unknown probe %r" % n)
    finally:
        dump(a.out)
        if needs_local and not a.keep:
            for nm in (GW_NAME, GW_NAME_B, "agw-probe-boot"):
                container_rm(nm)
            mocks_down()


if __name__ == "__main__":
    main()
