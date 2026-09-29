import json

import pytest

from fantasy_manager import prefs
from fantasy_manager.config import Settings, get_settings


@pytest.fixture
def env(monkeypatch, tmp_path):
    """Settings isolated from the real .env and data dir."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FANTRAX_MODE", raising=False)
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path / "data"))
    get_settings.cache_clear()
    yield tmp_path / "data"
    get_settings.cache_clear()


def test_get_set_roundtrip_and_atomic(env):
    assert prefs.get_pref("x", "fallback") == "fallback"          # missing file (and dir)
    prefs.set_pref("x", 3)
    prefs.set_pref("dynasty_mode", "rebuild")
    assert prefs.get_pref("x") == 3 and prefs.load_prefs() == {"x": 3, "dynasty_mode": "rebuild"}
    assert json.loads((env / "prefs.json").read_text(encoding="utf-8"))["dynasty_mode"] == "rebuild"
    assert [p.name for p in env.iterdir()] == ["prefs.json"]       # no temp files left behind
    prefs.set_pref("x", None)                                       # None removes the key
    assert prefs.load_prefs() == {"dynasty_mode": "rebuild"}


@pytest.mark.parametrize("content", ["{not json", "[1, 2]", "", "\x00\x01"])
def test_corrupt_file_is_tolerated(env, content):
    env.mkdir(parents=True)
    (env / "prefs.json").write_text(content, encoding="utf-8")
    assert prefs.load_prefs() == {} and prefs.get_pref("dynasty_mode", "d") == "d"
    assert prefs.dynasty_mode_info(Settings(_env_file=None)) == ("balanced", "default")
    prefs.set_pref("dynasty_mode", "contend")                      # overwrites the corrupt file
    assert prefs.get_pref("dynasty_mode") == "contend"


def test_mode_precedence_pref_over_env_over_default(env, monkeypatch):
    assert prefs.dynasty_mode_info(Settings(_env_file=None)) == ("balanced", "default")
    monkeypatch.setenv("FANTRAX_MODE", "Contend")
    s = Settings(_env_file=None)
    assert prefs.dynasty_mode_info(s) == ("contend", "env")
    prefs.set_pref("dynasty_mode", "rebuild", data_dir=s.fm_data_dir)
    assert prefs.dynasty_mode_info(s) == ("rebuild", "prefs")
    assert prefs.effective_dynasty_mode(s) == "rebuild"
    prefs.set_pref("dynasty_mode", "tank", data_dir=s.fm_data_dir)  # invalid pref is ignored
    assert prefs.dynasty_mode_info(s) == ("contend", "env")
    assert prefs.mode_source_label("prefs") == "dashboard/prefs"


def test_env_file_counts_as_env(env, tmp_path):
    (tmp_path / "mode.env").write_text("FANTRAX_MODE=rebuild\n", encoding="utf-8")
    s = Settings(_env_file=tmp_path / "mode.env")
    assert prefs.dynasty_mode_info(s) == ("rebuild", "env")


def test_defaults_to_global_settings_data_dir(env):
    prefs.set_pref("dynasty_mode", "contend")
    assert (env / "prefs.json").exists()
    assert prefs.effective_dynasty_mode() == "contend"
