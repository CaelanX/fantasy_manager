# Regenerates dotenv_golden.json from dotenv_golden.env using python-dotenv (the reference).
# Usage: python gen_dotenv_golden.py dotenv_golden.env dotenv_golden.json
import json, os, sys
os.environ["HOST_VAR"] = "from-proc"
from dotenv import dotenv_values
d = dotenv_values(sys.argv[1], encoding="utf8")
json.dump(d, open(sys.argv[2], "w", encoding="utf8"), indent=1, sort_keys=True, ensure_ascii=False)
