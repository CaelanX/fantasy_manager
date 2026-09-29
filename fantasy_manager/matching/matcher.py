"""Fuzzy player matching across sources (ESPN / Fantrax / NHL / news / injuries).

Rules (``match_player``):
  1. exact normalized name, team agrees               -> "exact"
  2. exact normalized name, unique among candidates   -> "exact"
     (several exact-name hits are tie-broken by team, then position -> "high"; else "pending")
  3. rapidfuzz token_sort_ratio >= 92 and team or position agrees -> "high"
  4. score in [85, 92), or >= 92 without any agreement            -> "pending"
  5. otherwise                                                     -> "none"
"""

from __future__ import annotations

from collections import defaultdict
from typing import Any, Iterable, Literal

from pydantic import BaseModel
from rapidfuzz import fuzz, process

from .normalize import normalize_name, normalize_team, positions_agree

Confidence = Literal["exact", "high", "pending", "none"]

HIGH_THRESHOLD = 92.0
PENDING_THRESHOLD = 85.0


class Candidate(BaseModel):
    key: Any  # hashable id in the candidate source (e.g. NHL player id)
    name: str
    team: str | None = None
    position: str | None = None


class MatchResult(BaseModel):
    key: Any = None
    name: str | None = None
    score: float = 0.0
    confidence: Confidence = "none"

    @property
    def matched(self) -> bool:
        """True for confident matches that can be auto-accepted."""
        return self.confidence in ("exact", "high")


def _team_agrees(a: str | None, b: str | None) -> bool:
    ta, tb = normalize_team(a), normalize_team(b)
    return bool(ta and tb and ta == tb)


class PlayerIndex:
    """Pre-normalized candidate pool for repeated lookups."""

    def __init__(self, candidates: Iterable[Candidate]):
        self.candidates: list[Candidate] = list(candidates)
        self.norm: list[str] = [normalize_name(c.name) for c in self.candidates]
        self.by_norm: dict[str, list[int]] = defaultdict(list)
        for i, n in enumerate(self.norm):
            self.by_norm[n].append(i)

    def __len__(self) -> int:
        return len(self.candidates)

    def _result(self, i: int, score: float, conf: Confidence) -> MatchResult:
        c = self.candidates[i]
        return MatchResult(key=c.key, name=c.name, score=round(score, 2), confidence=conf)

    def match(self, query_name: str, team: str | None = None, position: str | None = None,
              fuzzy_limit: int = 8) -> MatchResult:
        q = normalize_name(query_name)
        if not q or not self.candidates:
            return MatchResult()

        # ---- exact normalized-name hits
        exact = self.by_norm.get(q, [])
        if exact:
            if len(exact) == 1:
                return self._result(exact[0], 100.0, "exact")
            team_hits = [i for i in exact if _team_agrees(team, self.candidates[i].team)]
            if len(team_hits) == 1:
                return self._result(team_hits[0], 100.0, "exact")
            pool = team_hits or exact
            pos_hits = [i for i in pool if positions_agree(position, self.candidates[i].position)]
            if len(pos_hits) == 1:
                return self._result(pos_hits[0], 100.0, "exact" if team_hits else "high")
            # genuinely ambiguous duplicate name
            return self._result((pos_hits or pool)[0], 100.0, "pending")

        # ---- fuzzy
        hits = process.extract(q, self.norm, scorer=fuzz.token_sort_ratio,
                               limit=fuzzy_limit, score_cutoff=PENDING_THRESHOLD)
        if not hits:
            return MatchResult()

        def rank(hit: tuple[str, float, int]) -> tuple[float, float]:
            _, score, i = hit
            c = self.candidates[i]
            bonus = 3.0 * _team_agrees(team, c.team) + 1.0 * positions_agree(position, c.position)
            return (score + bonus, score)

        _, score, i = max(hits, key=rank)
        c = self.candidates[i]
        agrees = _team_agrees(team, c.team) or positions_agree(position, c.position)
        if score >= HIGH_THRESHOLD and agrees:
            return self._result(i, score, "high")
        return self._result(i, score, "pending")

    def match_many(self, queries: Iterable[tuple[str, str | None, str | None]]) -> list[MatchResult]:
        return [self.match(n, t, p) for n, t, p in queries]


def build_index(candidates: Iterable[Candidate]) -> PlayerIndex:
    return PlayerIndex(candidates)


def match_player(query_name: str, candidates: list[Candidate] | PlayerIndex,
                 team: str | None = None, position: str | None = None) -> MatchResult:
    index = candidates if isinstance(candidates, PlayerIndex) else PlayerIndex(candidates)
    return index.match(query_name, team=team, position=position)


def candidates_from(items: Iterable[Any], key: str = "player_id", name: str = "name",
                    team: str = "team", position: str = "position") -> list[Candidate]:
    """Build candidates from objects or dicts with the given attribute names."""
    out = []
    for it in items:
        get = it.get if isinstance(it, dict) else (lambda a, _it=it: getattr(_it, a, None))
        out.append(Candidate(key=get(key), name=get(name) or "", team=get(team), position=get(position)))
    return out


__all__ = ["Candidate", "MatchResult", "PlayerIndex", "build_index", "match_player", "candidates_from",
           "HIGH_THRESHOLD", "PENDING_THRESHOLD"]
