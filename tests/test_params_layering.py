"""valuation.params: packaged fit + harness override layering, reload, provenance, accessors."""
import json
import os

import pytest

from fantasy_manager.valuation import adjust, blend, schedule
from fantasy_manager.valuation import params as VP


@pytest.fixture
def pdir(tmp_path):
    """Enable the override layer against a temporary versions dir; restore afterwards."""
    d = tmp_path / "harness" / "params"
    d.mkdir(parents=True)
    old = {k: os.environ.get(k) for k in (VP.OVERRIDE_ENV, VP.PARAMS_DIR_ENV)}
    os.environ[VP.OVERRIDE_ENV] = "1"
    os.environ[VP.PARAMS_DIR_ENV] = str(d)
    VP.reload()
    yield d
    for k, v in old.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    VP.reload()


def _version(d, name, params, created="2026-11-17T09:00:00", activate=True):
    (d / f"{name}.json").write_text(json.dumps({"version": name, "created": created, "params": params,
                                                "status": "active"}), encoding="utf-8")
    if activate:
        (d / "active.json").write_text(json.dumps({"version": name}), encoding="utf-8")


def test_no_override_is_the_packaged_fit(pdir):
    packaged = VP.load_packaged()
    assert VP.load_params() == packaged
    assert VP.params_hash() == VP.params_hash(packaged)
    assert VP.source() == "fit 2026-09-28 (espn)" and VP.active_version() is None


def test_override_deep_merges_over_the_packaged_fit(pdir):
    _version(pdir, "v0003", {"inseason": {"k_skater": 30.0}, "projection": {"weight": 0.5},
                             "availability": {"dtd": {"week": 0.6}}, "schedule": {"offnight_bonus": 0.07}})
    merged = VP.reload()
    packaged = VP.load_packaged()
    assert merged["inseason"]["k_skater"] == 30.0
    assert merged["inseason"]["k_goalie"] == packaged["inseason"]["k_goalie"]              # sibling kept
    assert merged["inseason"]["recency_weights"] == packaged["inseason"]["recency_weights"]
    assert merged["preseason"] == packaged["preseason"] and merged["generated"] == packaged["generated"]
    assert VP.k_inseason("skater") == 30.0 and VP.k_inseason("G") == 40.0 and VP.k_inseason(True) == 40.0
    assert VP.projection_weight() == 0.5 and VP.offnight_bonus() == 0.07
    assert VP.availability("dtd", "week") == 0.6 and VP.availability("dtd", "season") == 0.75   # partial status
    assert VP.params_hash() == VP.params_hash(VP.deep_merge(packaged, VP.active_version()["params"]))
    assert VP.params_hash() != VP.params_hash(packaged)
    assert VP.source() == "fit 2026-09-28 (espn) + harness v0003 (2026-11-17)"


def test_live_code_reads_the_accessors_at_call_time(pdir):
    k0 = blend.K_INSEASON["skater"]
    _version(pdir, "v0001", {"inseason": {"k_skater": 22.0, "recency_weights": {"season": 0.8, "last30": 0.1,
                                                                                  "last15": 0.1, "last7": 0.0}},
                             "projection": {"k_goalie": 10.0},
                             "availability": {"out": {"week": 0.1, "season": 0.5}},
                             "schedule": {"offnight_bonus": 0.1, "start_share_k": 20, "start_share_prior": 0.4}})
    VP.reload()
    assert blend.shrink_k(False) == 22.0 and blend.projection_k(True) == 10.0
    assert blend.K_INSEASON["skater"] == k0                    # the alias is an import-time snapshot
    assert blend.recency_weights(13, 7, 4) == pytest.approx({"season": 0.8, "last30": 0.1, "last15": 0.1,
                                                             "last7": 0.0})
    assert blend.projection_blend_weight(40) == pytest.approx(0.6)       # not overridden
    assert adjust.availability_multiplier("out", "week") == 0.1 and adjust.availability_multiplier("out") == 0.5
    assert schedule.proj_week(2.0, 1.0, 4, 1) == pytest.approx(2.0 * 4 * 1.1)
    assert schedule.shrunk_share(10, 20) == pytest.approx((10 + 20 * 0.4) / (20 + 20))


def test_disable_flag_ignores_the_override(pdir):
    _version(pdir, "v0001", {"inseason": {"k_skater": 30.0}})
    VP.reload()
    assert VP.k_inseason("skater") == 30.0
    os.environ[VP.OVERRIDE_ENV] = "0"
    VP.reload()
    assert VP.k_inseason("skater") == 25.0 and VP.load_params() == VP.load_packaged()
    assert VP.source() == "fit 2026-09-28 (espn)" and not VP.override_enabled()


def test_reload_picks_up_a_new_version_and_a_rollback(pdir):
    _version(pdir, "v0001", {"inseason": {"k_skater": 30.0}})
    VP.reload()
    _version(pdir, "v0002", {"inseason": {"k_skater": 21.0}})
    assert VP.k_inseason("skater") == 30.0                     # cached until reload
    VP.reload()
    assert VP.k_inseason("skater") == 21.0
    (pdir / "active.json").write_text(json.dumps({"version": None}), encoding="utf-8")   # rolled back to packaged
    VP.reload()
    assert VP.k_inseason("skater") == 25.0 and VP.active_version() is None


@pytest.mark.parametrize("pointer,version_body", [
    ("{not json", None),
    (json.dumps({"version": "v0009"}), None),                                   # points to a missing file
    (json.dumps({"version": "../../etc"}), None),                               # not a version name
    (json.dumps({"version": "v0001"}), "{broken"),
    (json.dumps({"version": "v0001"}), json.dumps({"params": ["not", "a", "dict"]})),
    (json.dumps(["v0001"]), json.dumps({"params": {"inseason": {"k_skater": 30}}})),
])
def test_corrupt_override_is_tolerated(pdir, pointer, version_body):
    (pdir / "active.json").write_text(pointer, encoding="utf-8")
    if version_body is not None:
        (pdir / "v0001.json").write_text(version_body, encoding="utf-8")
    assert VP.reload() == VP.load_packaged()
    assert VP.k_inseason("skater") == 25.0 and VP.source() == "fit 2026-09-28 (espn)"


def test_bad_override_values_fall_back_per_accessor(pdir):
    _version(pdir, "v0001", {"inseason": {"k_skater": -3, "k_goalie": "x",
                                          "recency_weights": {"season": 0.9, "last30": 0.2}},
                             "projection": {"weight": 1.7, "k_skater": None},
                             "availability": {"dtd": [2.0, 0.5], "weird": "x"},
                             "schedule": {"offnight_bonus": float("nan"), "start_share_prior": 3}})
    VP.reload()
    assert VP.k_inseason() == VP.FALLBACK_K_INSEASON
    assert VP.recency_weights() == VP.FALLBACK_RECENCY                     # partial set -> whole fallback
    assert VP.projection_weight() == VP.FALLBACK_PROJECTION_WEIGHT
    assert VP.k_projection() == VP.FALLBACK_K_PROJECTION
    assert VP.availability("dtd", "week") == 0.75 and VP.availability("dtd", "season") == 0.5
    assert VP.offnight_bonus() == VP.FALLBACK_OFFNIGHT_BONUS and VP.start_share_prior() == 0.5


def test_accessors_fall_back_without_any_params():
    empty: dict = {}
    assert VP.k_inseason(params=empty) == VP.FALLBACK_K_INSEASON
    assert VP.k_inseason(empty) == VP.FALLBACK_K_INSEASON                  # pre-harness positional form
    assert VP.k_inseason("goalie", empty) == 40.0 and VP.k_projection("skater", empty) == 20.0
    assert VP.k_projection(empty) == {"skater": 20.0, "goalie": 12.0}
    assert VP.recency_weights(empty) == VP.FALLBACK_RECENCY
    assert VP.projection_weight(empty) == 0.6
    assert VP.availability_table(empty) == VP.FALLBACK_AVAILABILITY
    assert VP.availability("ir", "week", empty) == 0.0 and VP.availability("ir", "season", empty) == 0.4
    assert VP.availability("no-such-status", "week", empty) == 1.0
    assert VP.start_share_prior(empty) == 0.5 and VP.start_share_k(empty) == 10.0
    assert VP.offnight_bonus(empty) == 0.05
    assert VP.source(empty) == "built-in fallback"
    # the constants the live code used before the refactor are unchanged
    assert adjust.AVAILABILITY == VP.FALLBACK_AVAILABILITY
    assert (schedule.OFFNIGHT_BONUS, schedule.START_SHARE_K, schedule.START_SHARE_PRIOR) == (0.05, 10.0, 0.5)
    assert blend.K_PROJECTION == {"skater": 20.0, "goalie": 12.0} and blend.PROJECTION_WEIGHT == 0.6


def test_params_dir_default_and_env(tmp_path, monkeypatch):
    monkeypatch.delenv(VP.PARAMS_DIR_ENV, raising=False)
    assert VP.params_dir(tmp_path) == tmp_path / "harness" / "params"
    monkeypatch.setenv(VP.PARAMS_DIR_ENV, str(tmp_path / "x"))
    assert VP.params_dir() == tmp_path / "x"
