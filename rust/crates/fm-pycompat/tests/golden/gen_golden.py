"""Generate CPython golden cases for fm-pycompat (run with the project venv, CPython 3.12).

    C:/code/fantasy_manager/.venv/Scripts/python rust/crates/fm-pycompat/tests/golden/gen_golden.py

Writes <this dir>/*.json, read by ../golden.rs. Floats are transported as their IEEE-754 bit
pattern (an unsigned int) so nothing is lost on the way. Deterministic (fixed seeds).
"""
from __future__ import annotations

import json
import math
import random
import struct
import sys
import unicodedata
from datetime import date, datetime, timedelta
from pathlib import Path

OUT = Path(__file__).resolve().parent
assert sys.version_info[:2] >= (3, 12), "goldens must come from CPython 3.12+ (sum() is Neumaier)"

R = random.Random(20261104)


def bits(x: float) -> int:
    return struct.unpack("<Q", struct.pack("<d", x))[0]


def write(name: str, data) -> None:
    (OUT / f"{name}.json").write_text(json.dumps(data, separators=(",", ":"), ensure_ascii=True) + "\n",
                                      encoding="utf-8")


# ---------------------------------------------------------------------------------------------
# float corpus
SPECIAL = [0.0, -0.0, math.inf, -math.inf, math.nan, 5e-324, -5e-324, 2.2250738585072014e-308,
           1.7976931348623157e308, -1.7976931348623157e308, 0.5, 1.5, 2.5, 3.5, -0.5, -1.5, -2.5,
           0.125, 0.375, 0.625, -0.125, 2.675, 1.005, 1.115, 0.045, 1e-5, 9.999999999999999e-06,
           1e-4, 0.0001, 0.00011, 1e16, 9999999999999998.0, 1e15, 1e17, 123456789012345678.0, 0.1, 0.2,
           0.3, 1 / 3, 2 / 3, 100.0, 1e22, 1e23, 1e-7, 1.5e-7, 12345.678, 999999.5, 1e6, 999999.0,
           1234567.0, 0.000123456, 9.995, 99.95, 0.99995, 0.995, 0.05, 0.15, 0.25, 0.35, 0.45, 0.55,
           -0.05, -0.0049, -0.004, -0.001, 1e300, 1e-300, 4.35, 4.45, 2.0 ** 52 + 0.5, 2.0 ** 53,
           2.0 ** 63, 2.0 ** 64, 1234.5, 1235.0, 1225.0, 15.0, 25.0, -25.0, 0.0005, 0.0015, 0.0025,
           12.5, 0.6931471805599453, 3.141592653589793, 1.0541809484618251e-16, 982928183363351.25]


def rand_float() -> float:
    kind = R.randrange(10)
    if kind == 8:  # 1e14..1e17 on a coarse grid: ties in shortest-repr digit generation
        return R.randrange(10 ** 14, 10 ** 17) / R.choice([1, 2, 4, 8, 16])
    if kind == 9:  # few mantissa bits at any scale (exact ties in formatting)
        return R.randrange(1, 1 << R.randrange(1, 30)) * 2.0 ** R.randrange(-60, 60) * R.choice([1, -1])
    if kind == 0:  # arbitrary finite bit pattern
        while True:
            x = struct.unpack("<d", struct.pack("<Q", R.getrandbits(64)))[0]
            if math.isfinite(x):
                return x
    if kind == 1:  # a decimal with few digits (like stats / projections)
        return round(R.uniform(-1000, 1000), R.randrange(0, 5))
    if kind == 2:  # decimal ties: k + 0.5 / 10**n
        n = R.randrange(0, 4)
        return (R.randrange(-20000, 20000) + 0.5) / 10 ** n
    if kind == 3:  # small magnitudes
        return R.uniform(-1, 1) * 10.0 ** R.randrange(-12, 0)
    if kind == 4:  # large magnitudes
        return R.uniform(-1, 1) * 10.0 ** R.randrange(1, 30)
    if kind == 5:  # fractions in [0, 1] (percent formats)
        return R.random()
    if kind == 6:  # ratio of small ints
        return R.randrange(-300, 300) / R.randrange(1, 97)
    return R.gauss(0, 50)


FLOATS = SPECIAL + [rand_float() for _ in range(1200)]
FINITE = [x for x in FLOATS if math.isfinite(x)]

# ---------------------------------------------------------------------------------------------
# num: sum / round / floordiv / mod
sum_cases = []
for i in range(1500):
    n = R.randrange(0, 40)
    mode = i % 5
    if mode == 0:
        xs = [rand_float() for _ in range(n)]
    elif mode == 1:
        xs = [round(R.uniform(0, 10), 2) for _ in range(n)]
    elif mode == 2:
        xs = [R.choice([0.1, 0.2, 0.3, 1e16, -1e16, 1.0, 1e-16, 1e100, -1e100]) for _ in range(n)]
    elif mode == 3:
        xs = [R.uniform(-1, 1) * 10.0 ** R.randrange(-5, 20) for _ in range(n)]
    else:
        xs = [R.choice(SPECIAL) for _ in range(min(n, 6))]
    s = sum(xs)
    sum_cases.append({"xs": [bits(x) for x in xs], "sum": bits(s)})
sum_cases.append({"xs": [bits(0.1)] * 10, "sum": bits(sum([0.1] * 10))})
sum_cases.append({"xs": [bits(-0.0), bits(-0.0)], "sum": bits(sum([-0.0, -0.0]))})
sum_cases.append({"xs": [bits(1e308), bits(1e308), bits(-1e308)], "sum": bits(sum([1e308, 1e308, -1e308]))})

round_cases = []
for x in FLOATS:
    if math.isfinite(x):
        r0 = round(x)
        round_cases.append({"x": bits(x), "n": None, "r": bits(float(r0)), "i": r0 if abs(r0) < 2 ** 63 else None})
    for n in (0, 1, 2, 3, -1, R.randrange(-30, 30), R.randrange(4, 330)):
        round_cases.append({"x": bits(x), "n": n, "r": bits(round(x, n))})
for x in (1e-320, 5e-324, 1.5e-310):
    for n in (300, 320, 323, 324, 330, 400):
        round_cases.append({"x": bits(x), "n": n, "r": bits(round(x, n))})
for x in (1.7976931348623157e308, 1e308, 5e307):
    for n in (-305, -307, -308, -309, -400):
        try:
            round_cases.append({"x": bits(x), "n": n, "r": bits(round(x, n))})
        except OverflowError:
            round_cases.append({"x": bits(x), "n": n, "r": None})

div_cases = []
for _ in range(700):
    a, b = R.choice(FLOATS), R.choice(FLOATS + [2.0, -2.0, 7.0, -7.0, 0.1, 1.0, 3.0])
    if b == 0 or not (math.isfinite(a) and math.isfinite(b)):
        continue
    try:
        div_cases.append({"a": bits(a), "b": bits(b), "fd": bits(a // b), "md": bits(a % b)})
    except (OverflowError, ZeroDivisionError):
        pass
for a in (-7.0, 7.0, -0.0, 0.0, 6.0, -6.0, 1e-300, -1e-300, 5.5, -5.5):
    for b in (2.0, -2.0, 3.0, -3.0, 0.5, -0.5, 1e300):
        div_cases.append({"a": bits(a), "b": bits(b), "fd": bits(a // b), "md": bits(a % b)})
idiv_cases = [[a, b, a // b, a % b] for a in range(-13, 14) for b in (-5, -3, -1, 1, 2, 3, 7)]
idiv_cases += [[R.randrange(-10 ** 12, 10 ** 12), R.choice([-1, 1]) * R.randrange(1, 10 ** 6)] for _ in range(300)]
idiv_cases = [[a, b, a // b, a % b] for a, b, *_ in idiv_cases]

write("num", {"sum": sum_cases, "round": round_cases, "div": div_cases, "idiv": idiv_cases,
              "median": [{"xs": [bits(x) for x in xs], "m": bits(__import__("statistics").median(xs))}
                         for xs in ([R.choice(FINITE) for _ in range(R.randrange(1, 12))] for _ in range(300))]})

# ---------------------------------------------------------------------------------------------
# fmt: repr / format specs / %-formatting / thousands
SPECS = [".0f", ".1f", ".2f", ".3f", ".4f", ".12f", "+.0f", "+.1f", "+.2f", "+.4f", ".0%", ".1%",
         "+.2%", "+.0%", "g", ".4g", ".3g", ".1g", ".0g", ".10g", ".17g", "+g", ".2e", "e", "+.3e", ""]
fmt_cases = []
for x in FLOATS:
    out = {"x": bits(x), "repr": repr(x), "str": str(x), "f": {}}
    for spec in SPECS:
        if spec.endswith("f") and abs(x) > 1e30 and spec != ".2f":
            continue
        out["f"][spec] = format(x, spec)
    fmt_cases.append(out)

PCT_SPECS = [".2f", "+.1f", "+.2f", "+.0f", ".0f", ".1f", ".3f", "f", "+d", "02d", "d", "i", "5d",
             "-5d", "g", "s", "r", "5.1f", "-8.2f", "08.2f", " .2f", "e", ".3e", "E", "G", ".2g",
             "10.3g", "+.2e", "#.0f", "x"]
pct_cases = []
for x in FLOATS[:300] + [R.choice(FLOATS) for _ in range(300)]:
    for spec in PCT_SPECS:
        try:
            s = ("%" + spec) % x
        except (OverflowError, ValueError, TypeError):
            s = None
        pct_cases.append({"x": bits(x), "spec": spec, "s": s})

thousands = [0, 1, -1, 12, 999, 1000, -1000, 12345, 1234567, -1234567, 10 ** 15, -(10 ** 18),
             2 ** 63 - 1, -(2 ** 63)] + [R.randrange(-10 ** R.randrange(1, 18), 10 ** R.randrange(1, 18))
                                         for _ in range(300)]
int_specs = ["+d", "02d", "04d", "d", ","]
int_cases = [{"n": n, "f": {s: format(n, s) for s in int_specs}} for n in thousands]
repr_stress = [[bits(x), repr(x)] for x in (rand_float() for _ in range(15000)) if math.isfinite(x)]
write("fmt", {"floats": fmt_cases, "percent": pct_cases, "ints": int_cases, "reprs": repr_stress})

# ---------------------------------------------------------------------------------------------
# json.dumps
ALPHABET = ("abcXYZ019 _-'\"\\/\b\f\n\r\t\x00\x01\x1f\x7f\x80\xa0\xe9\xff\u0100\u03a3\u2028\u2029"
            "\ud7ff\ue000\uffff\U00010000\U0001F600\U0010FFFF")


def rand_str(n: int | None = None) -> str:
    n = R.randrange(0, 10) if n is None else n
    return "".join(R.choice(ALPHABET) if R.random() < 0.6 else chr(R.choice(
        [R.randrange(0x20, 0x7f), R.randrange(0x80, 0x800), R.randrange(0x800, 0xd800),
         R.randrange(0xe000, 0x10000), R.randrange(0x10000, 0x110000)])) for _ in range(n))


def rand_value(depth: int = 0):
    k = R.randrange(10 if depth < 3 else 7)
    if k == 0:
        return None
    if k == 1:
        return R.random() < 0.5
    if k == 2:
        return R.choice([0, 1, -1, 42, 2 ** 53 + 1, -(2 ** 63), 2 ** 64 - 1, R.randrange(-10 ** 9, 10 ** 9)])
    if k in (3, 4):
        return R.choice(FINITE)
    if k in (5, 6):
        return rand_str()
    if k in (7, 8):
        return [rand_value(depth + 1) for _ in range(R.randrange(0, 5))]
    return {rand_str(R.randrange(0, 4)): rand_value(depth + 1) for _ in range(R.randrange(0, 5))}


VALUES = [None, True, False, 0, 1.0, -0.0, 1e-05, 1e16, 0.1, "", "é", "\U0001F600", [], {}, [[]], {"": {}},
          {"b": 1, "a": [1, 2.5, {"c": None}], "é": "x"}, [1, [2, [3, []]], {}],
          {"z": {"y": {"x": []}}, "a": 1}]
VALUES += [rand_value() for _ in range(400)]
OPTS = [
    {}, {"indent": 2}, {"indent": 1}, {"indent": 0}, {"indent": 4, "sort_keys": True},
    {"sort_keys": True}, {"separators": [",", ":"]}, {"separators": [",", ":"], "sort_keys": True},
    {"ensure_ascii": False}, {"ensure_ascii": False, "indent": 2}, {"indent": 1, "sort_keys": True},
    {"indent": 2, "separators": [",", ": "]},
]
json_cases = []
for v in VALUES:
    for o in OPTS:
        kw = dict(o)
        if "separators" in kw:
            kw["separators"] = tuple(kw["separators"])
        json_cases.append({"v": json.dumps(v), "o": o, "s": json.dumps(v, **kw)})
str_cases = [{"s": s, "a": json.dumps(s), "u": json.dumps(s, ensure_ascii=False)}
             for s in [rand_str(R.randrange(0, 30)) for _ in range(500)] + [ALPHABET]]
write("json", {"dumps": json_cases, "strs": str_cases})

# ---------------------------------------------------------------------------------------------
# random.Random
SEEDS = [0, 1, 7, 42, 20260928, 20261102, -1, -7, -20260928, 2 ** 32 - 1, 2 ** 32, 2 ** 32 + 5,
         2 ** 40 + 123, 2 ** 63 - 1, -(2 ** 63), 12345678901234]
rand_cases = []
for seed in SEEDS:
    rng = random.Random(seed)
    ops = []
    for i in range(260):
        k = R.randrange(9)
        if k == 0:
            ops.append(["random", None, bits(rng.random())])
        elif k == 1:
            n = R.choice([0, 1, 5, 31, 32, 33, 53, 63, 64])
            ops.append(["getrandbits", n, rng.getrandbits(n)])
        elif k == 2:
            n = R.choice([1, 2, 3, 7, 10, 100, 2 ** 31, 2 ** 32 + 1, 2 ** 53, 2 ** 63])
            ops.append(["randbelow", n, rng._randbelow(n)])
        elif k == 3:
            a = R.randrange(-50, 50)
            b = a + R.randrange(1, 1000)
            ops.append(["randrange", [a, b], rng.randrange(a, b)])
        elif k == 4:
            xs = list(range(R.randrange(0, 20)))
            rng.shuffle(xs)
            ops.append(["shuffle", len(xs), xs])
        elif k == 5:
            n, kk = R.randrange(1, 300), R.randrange(0, 20)
            ops.append(["choices", [n, kk], rng.choices(range(n), k=kk)])
        elif k in (6, 7):
            mu, sigma = R.choice([(0.0, 1.0), (5.0, 2.5), (-3.0, 0.1), (100.0, 37.0)])
            ops.append(["gauss", [bits(mu), bits(sigma)], bits(rng.gauss(mu, sigma))])
        else:
            n = R.randrange(1, 50)
            ops.append(["choice", n, rng.choice(range(n))])
    rand_cases.append({"seed": seed, "ops": ops})
# long single streams (the Monte Carlo shapes)
streams = []
for seed in (7, 20260928, 20261102):
    rng = random.Random(seed)
    streams.append({"seed": seed, "kind": "random", "vals": [bits(rng.random()) for _ in range(5000)]})
    rng = random.Random(seed)
    streams.append({"seed": seed, "kind": "gauss", "vals": [bits(rng.gauss(1.5, 0.7)) for _ in range(5000)]})
write("random", {"ops": rand_cases, "streams": streams})

# ---------------------------------------------------------------------------------------------
# text
STRS = ["", "a", "abc", "héllo wörld", "Ünïcödé", "日本語テキスト", "😀a😀b", "McDAVID", "o'neil",
        "they're bill's", "ǆemal", "ßtraße", "ﬁsh ﬂow", "İstanbul", "ΣΊΣΥΦΟΣ", "hello WORLD 123abc",
        "  x  ", "a-b_c d.e", "1st 2nd", "MIXED case Words", "éclair", "ǅ", "ǈa ǋb", "ŉ", "\u0345x",
        "fantrax", "ESPN", "league", "jean-gabriel pageau", "r.j. o'brien", "pp1 unit"]
STRS += [rand_str(R.randrange(0, 15)) for _ in range(300)]
slices = []
for s in STRS:
    for _ in range(6):
        a = R.choice([None, R.randrange(-20, 20)])
        b = R.choice([None, R.randrange(-20, 20)])
        slices.append([s, a, b, s[a:b]])
DIGITS = ["", "0", "123", "12a", "²", "١٢٣", "½", "Ⅻ", "①", " 1", "1.5", "-1", "٣", "𝟗", "0x1"]
DIGITS += [rand_str(R.randrange(1, 4)) for _ in range(200)]


def ranges(pred):
    out, start = [], None
    for cp in range(0x110000):
        ok = pred(chr(cp))
        if ok and start is None:
            start = cp
        if not ok and start is not None:
            out.append([start, cp - 1])
            start = None
    if start is not None:
        out.append([start, 0x10FFFF])
    return out


title_diff = [[cp, chr(cp).title(), chr(cp).upper()] for cp in range(0x110000)
              if 0xd800 > cp or cp > 0xdfff
              if chr(cp).title() != chr(cp).upper()]
lower_map = [[cp, chr(cp).lower()] for cp in range(0x110000) if not (0xd800 <= cp <= 0xdfff)
             if chr(cp).lower() != chr(cp)]
upper_map = [[cp, chr(cp).upper()] for cp in range(0x110000) if not (0xd800 <= cp <= 0xdfff)
             if chr(cp).upper() != chr(cp)]
cased = ranges(lambda c: c.lower() != c.upper() or c.title() != c or unicodedata.category(c) in ("Lu", "Ll", "Lt"))
write("text", {
    "slices": slices,
    "case": [[s, s.capitalize(), s.title(), s.lower(), s.upper()] for s in STRS],
    "isdigit": [[s, s.isdigit()] for s in DIGITS],
    "isdigit_ranges": ranges(str.isdigit),
    "isspace_ranges": ranges(str.isspace),
    "split": [[s, s.split(), s.strip()] for s in STRS + ["a\x1cb\x1fc", " \u3000x\u2028y\x85 ", "\t\n a  b \r\n"]],
    "title_diff": title_diff, "lower_map": lower_map, "upper_map": upper_map,
    "unicode": unicodedata.unidata_version,
})

# ---------------------------------------------------------------------------------------------
# time
STRF = ["%H:%M UTC", "%Y%m%d", "%Y-%m-%d", "%Y-%m-%dT%H:%M:%SZ", "%a %b %d, %I:%M %p", "%a", "%b %d %H:%M UTC",
        "%A %b %d", "%A, %B %d, %Y", "%H:%M", "%a %b %d", "%b %d", "%b %d, %H:%M", "%b %d, %Y %H:%M", "%m/%d",
        "%y %j %I %p %S %%", "%B"]
dts = [datetime(2026, 1, 1, 0, 0, 0), datetime(2026, 11, 4, 12, 30, 5), datetime(2027, 2, 28, 23, 59, 59),
       datetime(2024, 2, 29, 12, 0, 0), datetime(1999, 12, 31, 0, 5, 9), datetime(2000, 7, 4, 13, 1, 2)]
dts += [datetime(2020, 1, 1) + timedelta(seconds=R.randrange(0, 10 ** 9)) for _ in range(120)]
strftime_cases = [[d.isoformat(), f, d.strftime(f)] for d in dts for f in STRF]

PFMT = ["%a %b %d, %Y", "%b %d/%y", "%b %d, %Y", "%B %d, %Y", "%b %d %Y", "%B %d %Y", "%Y-%m-%d", "%m/%d/%Y",
        "%m/%d/%y", "%d %b %Y", "%a %b %d, %Y, %I:%M%p", "%a %b %d, %Y, %I:%M %p", "%b %d, %Y, %I:%M%p",
        "%b %d, %Y, %I:%M %p", "%a %b %d, %I:%M%p %Y", "%a %b %d, %I:%M %p %Y", "%b %d, %I:%M%p %Y",
        "%b %d, %I:%M %p %Y", "%a %b %d %Y", "%b %d %Y", "%Y%m%d", "%H:%M"]
pinputs = []
for d in dts[:15]:
    for f in PFMT:
        pinputs.append(d.strftime(f))
        if R.random() < 0.3:
            pinputs.append(d.strftime(f).upper())
        if R.random() < 0.2:
            pinputs.append(d.strftime(f).lower().replace(" ", "  "))
pinputs += ["Thu Mar 4, 2027", "Mar 4, 2027", "March 4, 2027", "Sept 4, 2027", "Mar 32, 2027", "Feb 29, 2027",
            "Feb 29, 2028", "Feb 29 1900", "1/2/26", "01/02/2026", "13/02/2026", "Mon Sep 21, 2026, 10:15AM",
            "Sep 21, 10:15 AM 2026", "Sep 21, 12:00 AM 2026", "Sep 21, 12:00 PM 2026", "Sep 21, 0:00 PM 2026",
            "Mar 4/27", "4 Mar 2027", "2027-03-04", "2027-3-4", " Mar 4, 2027", "Mar 4, 2027 ", "Mar  4, 2027",
            "Mar 04, 2027", "Mar 4,2027", "mAR 4, 2027", "Thu Mar 4, 2027, 12:00PM", "Wed Mar 4, 2027",
            "Tue Sep 1, 2026, 7:05pm", "Tues Sep 1, 2026", "20270304", "2027034", "12:5", "1:05", "Mar 4 / 27",
            "Mar 4", "Dec 31 2026", "Jan 1 2027", "Mon Feb 29, 2028", "May 1, 2026", "June 1, 2026",
            "Jun 1, 2026", "Sep 9, 2026, 9:9AM", "Sep 9, 2026, 09:09 am", "Sep 9, 2026, 13:09 PM"]
strptime_cases = []
for s in pinputs:
    for f in PFMT:
        try:
            r = datetime.strptime(s, f).isoformat()
        except ValueError:
            r = None
        strptime_cases.append([s, f, r])

ISO = ["2026-11-04", "2026-11-04T10:00:00", "2026-11-04 10:00:00", "2026-11-04T10:00", "2026-11-04T10",
       "2026-11-04T10:00:00Z", "2026-11-04T10:00:00+00:00", "2026-11-04T10:00:00-05:00",
       "2026-11-04T10:00:00.123", "2026-11-04T10:00:00.123456", "2026-11-04T10:00:00.1234567",
       "2026-11-04T10:00:00,5", "2026-09-27T13:59+00:00", "2026-11-04T10:00:00+0530", "2026-11-04T10:00:00+05",
       "2026-11-04T10:00:00.5+01:30:15", "20261104", "20261104T100000", "2026-11-04T25:00:00", "2026-13-04",
       "2026-11-31", "2026-11-04X10:00", "2026-11-04T10:00:00 ", "", "2026", "2026-11", "2026-W45-3",
       "2026W453", "2026-11-04T10:00:00Z ", "2026-11-04T10:00:00z", "2026-11-04T100000", "2026-11-04T1000",
       "2026-11-04T10:00:60", "2026-11-04T24:00:00", "2026-11-04T10:00:00.", "2026-11-04T10:00:00+24:00",
       "2026-11-04T10:00:00-00:30", "2026-02-29", "2024-02-29T00:00:00", "2026-11-04T10:00:00.000001Z",
       "2026-11-04T10:00:00.12+00:00", "2026-11-04 10:00:00.123456+00:00", "0001-01-01", "9999-12-31T23:59:59"]
iso_cases = []
for s in ISO:
    try:
        dt = datetime.fromisoformat(s)
        off = dt.utcoffset()
        r = [dt.replace(tzinfo=None).isoformat(), None if off is None else int(off.total_seconds())]
    except ValueError:
        r = None
    try:
        dd = date.fromisoformat(s).isoformat()
    except ValueError:
        dd = None
    iso_cases.append([s, r, dd])
write("time", {"strftime": strftime_cases, "strptime": strptime_cases, "fromisoformat": iso_cases})

print("python", sys.version.split()[0], "unicode", unicodedata.unidata_version, "->", OUT)
