# Prints "KEYLEN <name> <len>" per key (names and lengths only) for a .env, via python-dotenv.
import os, sys
from dotenv import dotenv_values
d = dotenv_values(sys.argv[1], encoding="utf8")
for k in sorted(d):
    print("KEYLEN", k, -1 if d[k] is None else len(d[k]))
print("KEYCOUNT", len(d))
