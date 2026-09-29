"""Backtesting harness: NHL history -> projection models -> accuracy metrics -> fitted constants.

Modules:

* ``data``      per-player-season table (and in-season date windows) from the free NHL stats API
* ``scoring``   ESPN / Fantrax / default point presets and fantasy points per game
* ``models``    candidate preseason projection models (naive, marcel, fm_current, fm_fitted, fm_multi)
* ``evaluate``  preseason and in-season checkpoint evaluation (MAE, RMSE, Spearman, top-N)
* ``fit``       shrinkage k, in-season recency weights and an age curve fitted from data
* ``archive``   daily snapshots of provider projections and recommendations for later grading
* ``report``    markdown report writer
"""
