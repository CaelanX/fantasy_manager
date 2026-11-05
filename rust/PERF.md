# Python vs Rust: wall time on the real leagues

Measured 2026-11-05 (W11b) on the developer PC (Windows 11, 16 threads), from `C:\code\fantasy_manager`
with the real `.env` and `data/`. Python = `rust-port` branch `fantasy_manager/` (`.venv` Python 3.12);
Rust = `cargo build --release -p fm-cli` (`debug = 0`, 39 MB `fm.exe`). Each time is the whole process,
start to exit, `--json` output to /dev/null. Scripts: `tmp/w11b/perf.py`, `tmp/w11b/web_check.py` (job dir).

## CLI

Cold = `roster --json` against a copy of `data/` with no `cache.db` / `fantrax_cache` (every HTTP
request goes to the network). I ran it twice and report both runs. Warm = the real `data/` with a primed
HTTP cache; one priming run, then the median of 3.

| League  | Command                  | Python   | Rust    | Speed-up |
|---------|--------------------------|---------:|--------:|---------:|
| ESPN    | cold full load (network) | 102.8 / 91.9 s | 15.2 / 12.1 s | ~7x |
| Fantrax | cold full load (network) | 109.6 / 115.2 s | 34.8 / 31.2 s | ~3.4x |
| ESPN    | `--help` (startup)       | 0.54 s   | 0.18 s  | 3.1x |
| ESPN    | `settings`               | 1.31 s   | 0.38 s  | 3.5x |
| ESPN    | warm full load (`roster`)| 3.20 s   | 1.04 s  | 3.1x |
| ESPN    | `lineup`                 | 3.23 s   | 1.07 s  | 3.0x |
| ESPN    | `waivers`                | 3.20 s   | 1.05 s  | 3.1x |
| ESPN    | `matchup --seed 7`       | 3.30 s   | 1.05 s  | 3.1x |
| ESPN    | `claims`                 | 12.93 s  | 1.09 s  | 11.8x |
| ESPN    | `trades`                 | 11.36 s  | 1.10 s  | 10.3x |
| ESPN    | `plan --no-archive`      | 8.47 s   | 1.07 s  | 7.9x |
| ESPN    | `advise`                 | 14.29 s  | 1.09 s  | 13.1x |
| Fantrax | `settings`               | 1.06 s   | 0.41 s  | 2.6x |
| Fantrax | warm full load (`roster`)| 3.18 s   | 1.13 s  | 2.8x |
| Fantrax | `lineup`                 | 3.21 s   | 1.13 s  | 2.8x |
| Fantrax | `waivers`                | 3.20 s   | 1.15 s  | 2.8x |
| Fantrax | `matchup --seed 7`       | 3.24 s   | 1.15 s  | 2.8x |
| Fantrax | `claims`                 | 5.10 s   | 1.20 s  | 4.3x |
| Fantrax | `trades`                 | 10.21 s  | 1.25 s  | 8.2x |
| Fantrax | `plan --no-archive`      | 5.30 s   | 1.19 s  | 4.5x |
| Fantrax | `advise`                 | 10.81 s  | 1.23 s  | 8.8x |

Other live runs from the side-by-side sweep (one run each, so they are less precise): `harness daily` 58.9 s -> 22.6 s
(this includes network), `harness pregame` (both leagues) 35-47 s -> 19-21 s (network), `report` 23.4 s / 14.9 s ->
1.7 s, `backtest run` 26.4 s -> 3.3 s, `backtest fit` 9.2 s -> 2.9 s, `harness odds` 1.4 s -> 0.4 s.

Notes:
- Every warm Rust command costs about 1.05-1.25 s, and almost all of it is the shared league load (HTTP cache
  reads and JSON parse, enrichment, valuation). Startup is 0.18 s. The engines themselves (trades, plan,
  claims, advise) add under 0.1 s, where Python spends 2-11 s in them.
- Cold Fantrax in Rust is still 31-35 s. That is network-bound: the Fantrax fxpa calls and the sessions
  are sequential. ESPN cold drops from about 95 s to about 13 s because of the concurrent enrichment
  fan-out (`FM_FETCH_WORKERS`).
- Release debuginfo was removed (`[profile.release] debug = 0`). With `debug = 1`, the embedded
  windows-gnu debuginfo made `fm.exe` 230 MB and added about 0.8 s to every launch (`--help` took
  0.92 s; a stripped copy took 0.14 s).

## Dashboard (`fm web` vs `uvicorn fantasy_manager.web.app:app`)

The server ran on 127.0.0.1 against the real data with a warm HTTP cache, and I loaded each page for both
leagues. "First" is the first request after start, which includes the league load and the recommendations
when the page needs them. "Warm" is the best of 3 repeats. All 26 pages and the player page returned 200
with no error banner on both servers.

| Measure                                  | Python      | Rust        |
|------------------------------------------|------------:|------------:|
| process start -> `/healthz` answers      | 1.1 s       | 0.2 s       |
| ESPN `/plan`, first request              | 19.1 s      | 1.26 s      |
| Fantrax `/plan`, first request           | 12.4 s      | 3.58 s (both leagues warm up at start) |
| ESPN `/recommendations`, first request   | 15.3 s      | 0.19 s      |
| Fantrax `/recommendations`, first request| 6.5 s       | 0.24 s      |
| ESPN / Fantrax `/schedule`, first request| 1.28 / 0.34 s | 0.05 / 0.04 s |
| warm pages (plan, roster, matchup, schedule, news) | 4-30 ms | 3-16 ms |
| warm `/recommendations`                  | 87-106 ms   | 32-46 ms    |
| warm `/api/recs.json`                    | 89 ms       | 16 ms       |
