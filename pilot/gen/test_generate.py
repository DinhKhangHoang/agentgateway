"""Tests for the Kong -> agentgateway parity translator.

These run without a cluster: `extract()` is the only function that shells out
to kubectl, and every test here builds `Row` objects directly from the plugin
config shapes measured in `user-11377-maas-v2`.

Run: cd pilot/gen && python3 -m pytest test_generate.py -q
"""

import os
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import generate as g


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

# The two live path shapes, copied from `user-11377-maas-v2` HTTPRoutes:
# a shared-surface route, and a tenant-pinned one. Both are regex values, and
# the generator parses them as such -- fixtures that are plain paths would
# exercise a code path production never takes.
P = "/(?:maas/user-[^/]+/)?"
T = "/(?:maas/)?user-11377/"


def row(path, cfg, route="r", plugin="p"):
    r = g.Row(route, plugin, cfg, path)
    assert r.parsed, "fixture path %r did not parse; the test is wrong" % path
    return r


def chat_cfg(name="gpt-4o", provider="openai", **extra):
    cfg = {"route_type": "llm/v1/chat",
           "model": {"provider": provider, "name": name}}
    cfg.update(extra)
    return cfg


def build(rows):
    """classify + group, returning (report, backends, models)."""
    rep = g.Report()
    kept = g.classify(rows, [], {}, rep)
    backends, models = g.group(kept, rep)
    return rep, backends, models


def emitted(models, rep=None, insecure_tls=None):
    creds = g.Credentials()
    return g.emit_models(models, creds, "ns", "gw", "gwns", insecure_tls)


# --------------------------------------------------------------------------
# identity: what becomes a model, and what refuses to
# --------------------------------------------------------------------------

def test_chat_route_becomes_a_concrete_model():
    _, _, models = build([row(P + "openai/gpt-4o/v1/chat/completions$", chat_cfg())])
    matches = sorted(m["match"] for m in models)
    # Both forms: Kong's ai-routing pluginserver strips the vendor prefix
    # before dispatch, agentgateway matches `spec.match.model` verbatim.
    assert matches == ["gpt-4o", "openai/gpt-4o"]
    assert {m["provider"] for m in models} == {"OpenAI"}


def test_bare_form_is_withheld_when_two_vendors_share_a_model_name():
    _, _, models = build([
        row(P + r"z-ai/glm-5\.2/v1/chat/completions$", chat_cfg("glm-5.2"), route="a"),
        row(P + r"ksp/glm-5\.2/v1/chat/completions$", chat_cfg("glm-5.2"), route="b"),
    ])
    matches = sorted(m["match"] for m in models)
    # `resolve_concrete_model()` is a first-match find over a verbatim
    # comparison, so a bare `glm-5.2` CR would silently shadow one vendor.
    assert matches == ["ksp/glm-5.2", "z-ai/glm-5.2"]
    assert "glm-5.2" not in matches


def test_plugin_with_no_model_identity_anywhere_is_refused():
    # Measured on dev-v2: BYOK pass-throughs pin no model in the path and set
    # no config.model.name -- the client chooses. There is nothing to key an
    # AgentgatewayModel on, and defaulting to the plugin name would invent an
    # identifier no client sends.
    rep, _, models = build([row(P + "byok-openai/[^/]+/v1/chat/completions$",
                                {"route_type": "llm/v1/chat",
                                 "model": {"provider": "openai"}})])
    assert models == []
    assert rep.refused_identity, "a model with no identity must be recorded, not dropped"
    assert "config.model.name is unset" in rep.refused_identity[0][2]


def test_model_name_is_recovered_from_the_route_path_when_config_omits_it():
    # The other family with no config.model.name: identity is recoverable from
    # the path, which is what the client actually addresses.
    _, _, models = build([row(P + r"gemini/gemini-2\.5-flash-image/v1/generateContent$",
                              {"route_type": "llm/v1/generateContent",
                               "model": {"provider": "gemini"}})])
    assert "gemini/gemini-2.5-flash-image" in {m["match"] for m in models}


# --------------------------------------------------------------------------
# route types: the two paths the built-in table does not name
# --------------------------------------------------------------------------

def test_generate_content_opens_its_path():
    _, _, models = build([row(P + r"gemini/gemini-2\.5-pro/v1/generateContent$",
                              chat_cfg("gemini-2.5-pro", provider="gemini"))])
    assert all(m["paths"] == ["/v1/generateContent"] for m in models)
    assert "/v1/generateContent" in emitted(models)


def test_a_chat_only_model_opens_no_extra_path():
    _, _, models = build([row(P + "openai/gpt-4o/v1/chat/completions$", chat_cfg())])
    assert all(m["paths"] == [] for m in models)


def test_every_model_carries_the_route_table_not_just_the_one_that_opened_the_path():
    # The defect this pins: `spec.match.paths` is unioned across the listener
    # by Store::rebuild_model_router, but `spec.policies.routes` is per-model.
    # Emitting the table only on the opener leaves every sibling reachable on
    # the opened path and resolving it through ("*", Passthrough) -- routed but
    # never parsed, which is the FR-7.1 miss the table exists to close.
    _, _, models = build([
        row(P + r"qwen/qwen2-0\.5b/v1/completions$",
            chat_cfg("Qwen/Qwen2-0.5B"), route="opener"),
        row(P + "openai/gpt-4o/v1/chat/completions$", chat_cfg(), route="sibling"),
    ])
    opener = [m for m in models if m["match"] == "openai/gpt-4o"]
    assert opener and opener[0]["paths"] == [], "gpt-4o opens nothing itself"

    out = emitted(models)
    docs = [d for d in out.split("\n---\n") if "kind: AgentgatewayModel" in d]
    assert len(docs) == len(models)
    for d in docs:
        assert "    routes:" in d, "a model without a route table falls to Passthrough"
        assert "'/v1/completions': Detect" in d or "/v1/completions: Detect" in d


def test_the_emitted_table_restates_every_built_in_entry():
    # `merge_llm_policies` takes a non-empty preferred routes map WHOLE; it
    # does not merge. Emitting a partial table deletes the omitted types.
    _, _, models = build([row(P + r"qwen/qwen2-0\.5b/v1/completions$",
                              chat_cfg("Qwen/Qwen2-0.5B"))])
    out = emitted(models)
    for path, route_type in g.DEFAULT_ROUTE_TABLE:
        assert route_type in out
        assert path in out, "built-in path %r missing -> deleted for this model" % path


def test_non_builtin_paths_may_not_shadow_a_built_in_entry():
    builtin = {p for p, _ in g.DEFAULT_ROUTE_TABLE}
    assert not (g.NON_BUILTIN_PATHS & builtin), (
        "a path in both tables emits a duplicate YAML key; generate.py refuses "
        "at import time, so reaching this assertion means the guard was removed")


# --------------------------------------------------------------------------
# losses: things emitted with a feature dropped, and recorded
# --------------------------------------------------------------------------

def test_web_search_is_detected_when_nested_under_fallbacks():
    # Regression: a top-level scan of config.model missed
    # 11377-maas-no-delete-glm-5.2-model, which carries web_search only at
    # config.fallbacks[0].model.web_search. EVALUATION.md warned about this
    # exact nesting and it happened anyway, hence the full walk.
    r = row(P + r"z-ai/glm-5\.2/v1/chat/completions$",
            chat_cfg("glm-5.2", fallbacks=[
                {"model": {"provider": "openai", "name": "glm-5.2-backup",
                           "web_search": {"enabled": True}}}]))
    assert [k for k, _ in r.unsupported_features] == ["web_search"]


def test_web_search_at_any_of_the_three_measured_depths_is_detected():
    for cfg in (
        chat_cfg("m", **{"web_search": {"enabled": True}}),
        {"route_type": "llm/v1/chat",
         "model": {"provider": "openai", "name": "m",
                   "web_search": {"enabled": True}}},
        {"route_type": "llm/v1/chat",
         "model": {"provider": "openai", "name": "m",
                   "options": {"web_search": {"enabled": True}}}},
    ):
        r = row(P + "openai/m/v1/chat/completions$", cfg)
        assert [k for k, _ in r.unsupported_features] == ["web_search"], cfg


def test_a_disabled_feature_is_not_reported_as_a_loss():
    r = row(P + "openai/m/v1/chat/completions$",
            chat_cfg("m", **{"model": {"provider": "openai", "name": "m",
                                       "web_search": {}}}))
    assert r.unsupported_features == []


def test_web_search_model_is_emitted_with_the_feature_dropped_and_recorded():
    # A deliberate deviation from the original plan, which said refuse. A
    # refused model is an outage; a recorded loss is a decision the operator
    # can act on. The report is what makes it a decision rather than a silent
    # drop -- so the model MUST still appear, and the loss MUST be recorded.
    rep, _, models = build([row(P + "openai/m/v1/chat/completions$",
                                chat_cfg("m", **{"model": {
                                    "provider": "openai", "name": "m",
                                    "web_search": {"enabled": True}}}))])
    assert "openai/m" in {m["match"] for m in models}
    assert rep.unsupported, "dropping web_search silently defeats the report"
    assert "web_search" in rep.unsupported[0][2]


def test_tenant_pinned_routes_get_no_canonical_model():
    # The credential on a tenant-pinned route is that tenant's. A
    # body-`model`-routed AgentgatewayModel carries no tenant, so publishing
    # one would serve that tenant's key to every caller.
    rep, backends, models = build([
        row(T + "openai/gpt-4o/v1/chat/completions$", chat_cfg())])
    assert models == []
    assert backends, "the legacy per-tenant surface still carries it"
    assert any("Tenant-pinned" in h for h in rep.losses)


# --------------------------------------------------------------------------
# the drift detector
# --------------------------------------------------------------------------

FORK = os.path.expanduser("~/agentgateway")


@pytest.mark.skipif(not os.path.isdir(FORK), reason="no agentgateway checkout")
def test_default_route_table_matches_the_fork():
    assert g.parse_fork_default_table(FORK) == g.DEFAULT_ROUTE_TABLE


def _run_check(src):
    return subprocess.run(
        [sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                      "generate.py"),
         "--check-default-table", src],
        capture_output=True, text=True)


@pytest.mark.skipif(not os.path.isdir(FORK), reason="no agentgateway checkout")
def test_drift_detector_reports_a_changed_route_type(tmp_path):
    src = tmp_path / g._DEFAULT_TABLE_SRC
    src.parent.mkdir(parents=True)
    real = open(os.path.join(FORK, g._DEFAULT_TABLE_SRC), encoding="utf-8").read()
    src.write_text(real.replace('strng::new("/v1/rerank"), llm::RouteType::Rerank',
                                'strng::new("/v1/rerank"), llm::RouteType::Detect'))
    p = _run_check(str(tmp_path))
    assert p.returncode != 0
    assert "TYPE differs" in p.stdout + p.stderr


@pytest.mark.skipif(not os.path.isdir(FORK), reason="no agentgateway checkout")
def test_drift_detector_reports_an_entry_we_would_delete(tmp_path):
    src = tmp_path / g._DEFAULT_TABLE_SRC
    src.parent.mkdir(parents=True)
    real = open(os.path.join(FORK, g._DEFAULT_TABLE_SRC), encoding="utf-8").read()
    src.write_text(real.replace(
        '(strng::new("/v1/rerank"), llm::RouteType::Rerank),',
        '(strng::new("/v1/rerank"), llm::RouteType::Rerank),\n'
        '\t\t\t(strng::new("/v1/audio/speech"), llm::RouteType::Detect),'))
    p = _run_check(str(tmp_path))
    assert p.returncode != 0
    assert "would be DELETED from every emitted model" in p.stdout + p.stderr


@pytest.mark.skipif(not os.path.isdir(FORK), reason="no agentgateway checkout")
def test_drift_detector_fails_loudly_when_its_own_parser_goes_stale(tmp_path):
    # A checker that quietly finds nothing and reports "no drift" is worse
    # than no checker, because it would be trusted.
    src = tmp_path / g._DEFAULT_TABLE_SRC
    src.parent.mkdir(parents=True)
    real = open(os.path.join(FORK, g._DEFAULT_TABLE_SRC), encoding="utf-8").read()
    src.write_text(real.replace("pub fn default_route_types",
                                "pub fn builtin_route_types"))
    p = _run_check(str(tmp_path))
    assert p.returncode != 0
    assert "moved or renamed" in p.stdout + p.stderr


def test_drift_detector_reports_a_missing_checkout(tmp_path):
    p = _run_check(str(tmp_path / "nope"))
    assert p.returncode != 0
    assert "cannot read" in p.stdout + p.stderr


# --------------------------------------------------------------------------
# upstream TLS: --insecure-upstream-tls is opt-in and host-scoped
# --------------------------------------------------------------------------

# One host from the measured set, and one that verifies fine today.
PRIVATE_CA_URL = "https://49.213.86.184.nip.io/v1/maas/zai-org/glm-5.2/v1/chat/completions"
PUBLIC_CA_URL = "https://api.deepseek.com/v1/chat/completions"


def _model_with_upstream(url, name="glm-5.2", vendor="z-ai"):
    cfg = chat_cfg(name)
    cfg["model"]["options"] = {"upstream_url": url}
    path = P + vendor + "/" + name.replace(".", r"\.") + "/v1/chat/completions$"
    _, backends, models = build([row(path, cfg)])
    return backends, models


def test_no_tls_stanza_is_emitted_unless_the_flag_is_given():
    # The default must stay byte-identical to what is deployed: agentgateway
    # verifies unless told otherwise, and that default is the secure one.
    _, models = _model_with_upstream(PRIVATE_CA_URL)
    assert "insecureSkipVerify" not in emitted(models)


def test_flag_disables_verification_only_for_measured_private_ca_hosts():
    _, models = _model_with_upstream(PRIVATE_CA_URL)
    out = emitted(models, insecure_tls="All")
    assert "    tls:" in out
    assert "      insecureSkipVerify: All" in out


def test_flag_leaves_a_publicly_verifiable_host_verifying():
    # A blanket disable would also cover api.deepseek.com, which handshakes
    # fine -- and would then stop reporting if that ever changed.
    _, models = _model_with_upstream(PUBLIC_CA_URL, name="deepseek-v4", vendor="deepseek")
    assert "insecureSkipVerify" not in emitted(models, insecure_tls="All")


def test_backend_providers_keep_bare_tls_when_the_flag_is_off():
    # `tls: {}` still has to be emitted for an https host override, or the
    # gateway speaks plaintext to :443. The flag replaces it, never removes it.
    backends, _ = _model_with_upstream(PRIVATE_CA_URL)
    creds = g.Credentials()
    out = g.emit_backends(backends, creds, "ns", g.Report())
    assert "tls: {}" in out
    assert "insecureSkipVerify" not in out


def test_backend_providers_honour_the_flag():
    backends, _ = _model_with_upstream(PRIVATE_CA_URL)
    creds = g.Credentials()
    out = g.emit_backends(backends, creds, "ns", g.Report(), "All")
    assert "insecureSkipVerify: All" in out
    assert "tls: {}" not in out


# --------------------------------------------------------------------------
# Kong `fallbacks` on the legacy surface: which row declares them
# --------------------------------------------------------------------------

MODELARTS = "https://api-ap-southeast-1.modelarts-maas.com/v2/chat/completions"
SELFHOST = "https://49.213.86.184.nip.io/v1/maas/zai-org/glm-5.2/v1/chat/completions"


def _glm_cfg(route_type, upstream, fallback_url=None):
    cfg = {"route_type": route_type,
           "model": {"provider": "openai", "name": "zai-org/GLM-5.2-FP8",
                     "options": {"upstream_url": upstream}}}
    if fallback_url:
        cfg["fallbacks"] = [{"model": {"provider": "openai", "name": "glm-5.2",
                                       "options": {"upstream_url": fallback_url}}}]
    return cfg


def _glm_rows(chat_fallback=MODELARTS, responses_fallback=MODELARTS):
    # The live shape: three plugins share one upstream, and the messages plugin
    # -- which sorts first -- is the one WITHOUT a fallback.
    return [
        # Route and plugin names copied from the live surface, because the
        # ordering is the whole point: rows sort by route name, and
        # `...-messages-model-route` sorts before `...-model-route`, so the row
        # WITHOUT a fallback lands at rows[0].
        row(P + r"z-ai/glm-5\.2/v1/messages$",
            _glm_cfg("llm/v1/messages", SELFHOST),
            route="glm-5.2-messages-model-route", plugin="glm-5.2-messages-model"),
        row(P + r"z-ai/glm-5\.2/v1/chat/completions$",
            _glm_cfg("llm/v1/chat", SELFHOST, chat_fallback),
            route="glm-5.2-model-route", plugin="glm-5.2-model"),
        row(P + r"z-ai/glm-5\.2/v1/responses$",
            _glm_cfg("llm/v1/responses", SELFHOST, responses_fallback),
            route="glm-5.2-responses-model-route", plugin="glm-5.2-responses-model"),
    ]


def test_a_fallback_declared_on_a_later_row_still_becomes_a_priority_group():
    # The defect this pins: emit_backends read rows[0] only, and rows[0] here
    # is the messages plugin with no fallbacks -- so ModelArts vanished while
    # report.md kept claiming it survived on the legacy backend.
    rep, backends, _ = build(_glm_rows())
    # Precondition: without this the test would pass even with the defect.
    assert len(backends) == 1 and len(backends[0].rows) == 3
    assert not backends[0].rows[0].cfg.get("fallbacks"), \
        "fixture no longer reproduces the live row order; the test proves nothing"
    out = g.emit_backends(backends, g.Credentials(), "ns", rep)
    assert "api-ap-southeast-1.modelarts-maas.com" in out
    assert "Kong fallback #1 -> lower-priority group." in out


def test_rows_disagreeing_on_the_chain_emit_no_fallback_and_say_so():
    # `spec.ai.groups` is one ordered list per backend, so two different chains
    # cannot both be honoured. Refuse rather than silently pick one.
    rep, backends, _ = build(_glm_rows(responses_fallback="https://other.example/v1/chat/completions"))
    out = g.emit_backends(backends, g.Credentials(), "ns", rep)
    assert "modelarts-maas.com" not in out
    assert "other.example" not in out
    heading = "Rows sharing a backend declare DIFFERENT Kong fallback chains"
    assert heading in rep.losses
    assert "glm-5.2-model" in rep.losses[heading][0]
    assert "glm-5.2-responses-model" in rep.losses[heading][0]



def test_chains_differing_only_by_an_unconsumable_knob_still_emit():
    # The live GLM-5.2 shape: the chat and responses plugins name the SAME
    # ModelArts target, but the responses one also sets
    # `responses_upstream_format`. Comparing whole dicts called those two
    # chains different and emitted neither -- a silent failover loss dressed up
    # as a conflict. The knob has no agentgateway equivalent and is already
    # reported under unsupported features, so it must not veto the chain.
    rows = _glm_rows()
    fb = rows[2].cfg["fallbacks"][0]
    fb["model"]["options"]["responses_upstream_format"] = "chat"
    rep, backends, _ = build(rows)
    chats = [r for r in backends[0].rows if r.cfg["route_type"] == "llm/v1/chat"]
    assert chats and "responses_upstream_format" not in \
        chats[0].cfg["fallbacks"][0]["model"]["options"], \
        "fixture no longer differs between the two chains; the test proves nothing"
    out = g.emit_backends(backends, g.Credentials(), "ns", rep)
    assert "api-ap-southeast-1.modelarts-maas.com" in out
    assert "Rows sharing a backend declare DIFFERENT Kong fallback chains" \
        not in rep.losses
