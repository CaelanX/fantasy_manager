import pytest

from fantasy_manager.models import ScoringConfig
from fantasy_manager.scoring import CategoriesScoring, PointsScoring, RotoScoring, from_config


def test_points_weighted_sum():
    s = PointsScoring({"G": 3, "A": 2, "SOG": 0.4, "HIT": 0.3})
    assert s.value({"G": 0.5, "A": 1.0, "SOG": 4.0, "HIT": 2.0}) == pytest.approx(1.5 + 2 + 1.6 + 0.6)


def test_points_ignores_missing_and_unweighted_stats():
    s = PointsScoring({"G": 3, "BLK": 0.5})
    assert s.value({"G": 1.0, "PIM": 10.0}) == pytest.approx(3.0)
    assert s.value({}) == 0.0


def test_negative_weights_goalie():
    s = PointsScoring({"W": 4, "GA": -2, "SV": 0.2, "SO": 3})
    assert s.value({"W": 0.5, "GA": 2.5, "SV": 27.0, "SO": 0.1}) == pytest.approx(2 - 5 + 5.4 + 0.3)


def test_from_config_dispatch():
    assert isinstance(from_config(ScoringConfig(kind="points", weights={"G": 1})), PointsScoring)
    cat = from_config(ScoringConfig(kind="categories", categories=["G", "A"]))
    assert isinstance(cat, CategoriesScoring)
    assert isinstance(from_config(ScoringConfig(kind="roto", categories=["G"])), RotoScoring)
    assert cat.value({"G": 1.0}) == 0.0  # unfitted categories scoring has no reference pool
