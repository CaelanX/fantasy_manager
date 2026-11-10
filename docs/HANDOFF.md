# Handoff: engine accuracy work, 2026-11-10

Work in progress on the branch `wip/engine-kb-handoff`, written so a fresh session on another machine can continue. Delete this file when the work it describes is merged.

## To resume

Open the repository on this branch and tell the assistant:

> Read docs/HANDOFF.md and docs/knowledge/README.md, then continue from "Next steps" in the handoff.

## What is on this branch

**Finished and tested**

- **Local model support and the Ask chat.** `FM_LLM_BASE_URL` points the LLM client at any OpenAI-compatible server (Ollama). The dashboard has an Ask tab with saved chats, streamed answers, question-focused context, and Ask boxes on the player card, moves, trade and compare pages. Details are in the README.
- **Goalie start share, late arrivals.** A goalie who first appears for a team after it has played five or more games is measured from that first appearance (`crates/fm-harness/src/deployment.rs`, `TeamGoalieCounts::games_for`).
- **Goalie start share, injuries.** Games a goalie missed while out, on IR or suspended no longer count against his share (`TeamGoalieCounts::games_available`, fed from the status history in `crates/fm-providers/src/enrich.rs`).
- **The knowledge base** in `docs/knowledge/`: eight research files, the gap analysis (`09`), the Fantrax official API reference and its integration plan.

**Half-finished: do not trust yet**

- The first engine fix, gap A2 (constants assume an 82-game season; this season has 84), was being implemented when the run was stopped. Its unreviewed draft is in `crates/fm-engine/src/valuation/{valuate,adjust,schedule,picks}.rs`, `crates/fm-engine/src/recommend/{base,injuries}.rs` and the tests `crates/fm-engine/tests/{valuation_base,recommend_base}.rs`.
- The tree builds. `cargo test --workspace` has one failure, `stage_dynasty_fantrax` in `crates/fm-parity/tests/stage_dynasty.rs`, which compares Fantrax dynasty values with a stored fixture. It passed before the A2 draft. Whether the new values are the intended effect of A2 or a mistake in the draft has not been determined.

## Next steps

1. **Finish wave 1 of the gap fixes.** `docs/handoff/engine-gap-fixes-wave-1.workflow.js` is the pipeline that was running: nine units in sequence (A2, A1, B2, B5, B3, B1, B7 with B9, C2 with the small items, then corrections to the knowledge files). Each unit is implemented, reviewed from two angles, fixed and gated on the full test suite. Its first unit already knows about the A2 draft. Run it with the Workflow tool (`scriptPath`, and `args: {repo: "<path>"}` if the repository is not at `C:\code\personal\fantasy_manager`), or work through the units by hand from the briefs inside it. It uses Opus for implementation and Sonnet for reviews. Estimated five to nine hours of wall-clock time.
2. **Build the Fantrax official API integration** from `docs/knowledge/fantrax-official-api-plan.md`. Eleven small steps; the probes in step 0 are done. It was held back because it edits the same tree as the gap fixes, so run it after wave 1, not alongside it.
3. **Wave 2 of the gap fixes,** the modelling changes that need backtest evidence before they are kept: B6 (dynasty value over positional replacement, goalie curve and pedigree), C1 (goalie wins and shots from the team), B4 (project from ice time and power-play share), B8 (shrinkage constant per statistic and per scoring preset). All are described in `docs/knowledge/09-engine-gap-analysis.md`.
4. **Smaller items noticed along the way**
   - `fm playoffs -l fantrax --json` took more than five minutes; the ESPN call returns promptly. Cause not investigated.
   - `crates/fm-web/src/app.rs` has a text that says Fantrax locks lineups on Monday. This league locks daily.
   - `README.md` and `docs/hosting.md` say Fantrax has no API. It has one (see the reference file).
   - The Ask chat sends up to six named players in detail; a long paste plus many players can exceed a 16k-token context window.

## Setting up the other machine

The `data/` folder is in git on this branch (harness ledger, status history, crosswalk, archives, prefs, watchlist, saved chats), so the goalie start-share fixes have their history on any machine. Not in git: `.env` (league ids, cookies, model settings), `target/`, the HTTP caches (`data/cache.db`, `data/fantrax_cache/`, rebuilt on the first load) and the Fantrax login session (`data/fantrax_session.json`, so the other machine logs in again).

- **Build:** `cargo build --release -p fm-cli`, then `target/release/fm web`.
- **One machine at a time:** `data/harness.db` is a binary file, so two machines both running the daily task and committing it will conflict. Run the daily task on one machine, or pull before it runs.
- **Local model for the Ask tab (optional):** install Ollama 0.40.4 or later, run `ollama pull ifm/k2-horizon:7b`, set `OLLAMA_CONTEXT_LENGTH=16384` (and `OLLAMA_IGPU_ENABLE=1` on a PC with only integrated graphics), restart Ollama, and add `FM_LLM_BASE_URL=http://localhost:11434/v1` and `FM_LLM_MODEL=ifm/k2-horizon:7b` to `.env`.

## League facts the owner confirmed

- The Fantrax league keeps three players per team between seasons. Fantrax labels the league type "Dynasty".
- Both leagues' settings, playoff formats and deadlines are in `docs/knowledge/README.md`.

## How the owner wants this work done

These were given during the session and live in the assistant's memory on the first machine only, so they are repeated here.

- When something is found that makes player values, trade verdicts or recommendations wrong, verify it against the code and real league data, fix it, test it and report what moved. Do not write it up as "not fixed" and ask.
- The knowledge base is working input: read the relevant file and the gap entry before changing the engine, and correct a knowledge file in the same change when the code or data shows it is wrong.
- Check the league data (`fm settings`, `fm outlook`, `fm playoffs`, `fm claims`, all with `--json`) before calling a league fact unknown.
- Run well-defined delegated work on Opus or Sonnet. The top model is too expensive for it.
- Do not commit or push unless asked in the current conversation, and never add assistant attribution to commit messages.
- A usage limit stalls background runs silently; they resume only after a new message.
