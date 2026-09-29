"""Suite-wide setup.

The valuation params layer merges ``<fm_data_dir>/harness/params/active.json`` (a harness
refit version) over the packaged fit. Tests must not depend on whatever version is active in
the developer's real data dir, so the override layer is disabled for the whole session; the
params-layering and refit tests switch it back on against a temporary directory
(``params_override`` fixture in those modules).
"""
import os

os.environ["FM_PARAMS_OVERRIDE"] = "0"
os.environ.pop("FM_PARAMS_DIR", None)
