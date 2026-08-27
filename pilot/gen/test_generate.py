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


def emitted(models, rep=None):
    creds = g.Credentials()
    return g.emit_models(models, creds, "ns", "gw", "gwns")


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
