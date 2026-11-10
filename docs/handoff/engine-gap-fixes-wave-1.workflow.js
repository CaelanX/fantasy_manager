export const meta = {
  name: 'engine-gap-fixes-wave-1',
  description: 'Fix the engine gaps from docs/knowledge/09 one at a time: implement, review from two angles, apply findings, run the full test suite',
  phases: [
    { title: 'Implement', detail: 'one implementer per gap, strictly in sequence' },
    { title: 'Review', detail: 'two independent reviewers per gap: correctness, and research intent with real data' },
    { title: 'Fix and gate', detail: 'apply confirmed review findings, run the whole test suite' },
  ],
}

// Pass the repository path as the workflow's args.repo when it differs on this machine.
const REPO = (typeof args === 'object' && args && args.repo) || 'C:\\code\\personal\\fantasy_manager'
const KB = REPO + '\\docs\\knowledge'
// Before/after captures go under target\\, which git ignores.
const SCRATCH = REPO + '\\target\\handoff-scratch\\wave1'

const RULES = `PROJECT: ${REPO} is a Rust workspace ("fm"), a fantasy hockey manager for two head-to-head points leagues: an ESPN 14-team redraft league and a Fantrax 12-team league where each team keeps its top 3 players (owner-confirmed). It was ported from Python; many tests compare against Python output ("parity"), and most parity fixtures are absent locally so those tests skip.

KNOWLEDGE BASE: ${KB}\\README.md is the index (it holds both leagues' confirmed settings). ${KB}\\09-engine-gap-analysis.md lists gaps between published fantasy research and this engine, each with file:line references. Files 01 to 08 are the research. The owner's instruction is to fix the issues that analysis raised, and that anything making player values, trade verdicts or move recommendations wrong should be fixed when found.

HARD RULES
- Work directly in ${REPO}. Other agents work on this tree before and after you, never at the same time, so leave it compiling with all tests passing.
- Never run git commands that change anything: no commit, add, stash, checkout, reset, restore, clean, rebase, merge. Read-only git (status, diff, log, show) is allowed.
- Never read .env or any secret-bearing file, never print secret values.
- Never run commands that write league or harness data: no "fm sync --refresh", no "fm harness daily|pregame|refit|rollback|auto-apply|rebuild", no "fm mode <x>", no "fm backtest fit|archive", no "fm report --notify", no "fm notify". Read-only fm commands (settings, roster, lineup, waivers, trades, trade-eval, outlook, playoffs, claims, matchup, plan, goalies, keepers, picks, harness status|grade|review, backtest run|report) are fine. Do not edit files under ${REPO}\\data.
- Build and run only the DEBUG binary: "cargo build -p fm-cli" gives ${REPO}\\target\\debug\\fm.exe. Do not build or run the release profile (another process uses target\\release\\fm.exe).
- Do not install packages or add dependencies. Enabling a feature of an existing dependency is acceptable only if unavoidable; say so in your report.
- Make the smallest change that correctly fixes the issue and fits the existing patterns (tunable constants live with the others, for example crates\\fm-engine\\src\\valuation\\params.rs; reasons are added with Reason::new(code, text)). No unrelated refactors, renames or formatting churn. Match the file's style and comment density; comments explain non-obvious assumptions and units, not the history of the change.
- Both leagues must keep working. A rule that only makes sense for one scoring system must be expressed in terms of the league's own settings, not hard-coded per provider.
- Where the change deliberately departs from the original Python behaviour, say so in the function's doc comment in one sentence, and update any test that pinned the old behaviour to the new correct value (never loosen an assertion just to make it pass).
- Never invent a constant without a stated basis. A number must come from the research files (cite which), from the league's settings, from the repo's own fitted parameters or backtest, or be derived in a comment.
- Write for a reader who has not seen this task: plain complete sentences in comments, docs and reports.`

const IMPLEMENT_SCHEMA = {
  type: 'object',
  properties: {
    status: { type: 'string', enum: ['fixed', 'partly_fixed', 'not_a_real_gap', 'blocked'] },
    summary: { type: 'string', description: 'Up to 200 words: what was wrong, what you changed, and the effect on real league data' },
    claim_check: { type: 'string', description: 'What you verified in the code before changing anything, with file:line, and whether the gap analysis was right' },
    files_changed: {
      type: 'array',
      items: {
        type: 'object',
        properties: { file: { type: 'string' }, functions: { type: 'string' }, what: { type: 'string' } },
        required: ['file', 'functions', 'what'],
      },
    },
    tests: { type: 'string', description: 'Tests added or changed, and the result of cargo test --workspace (counts, any failures)' },
    real_data_effect: { type: 'string', description: 'Before/after on the real leagues: which players, verdicts or recommendations moved, with numbers, and the hand check you did' },
    constants_introduced: { type: 'string', description: 'Every new or changed constant and its basis; "none" if none' },
    left_undone: { type: 'string', description: 'Anything in the gap entry you did not do, and why' },
  },
  required: ['status', 'summary', 'claim_check', 'files_changed', 'tests', 'real_data_effect', 'constants_introduced', 'left_undone'],
}

const REVIEW_SCHEMA = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['sound', 'needs_fixes', 'wrong_should_be_reverted'] },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          where: { type: 'string', description: 'file:line' },
          problem: { type: 'string' },
          failure_scenario: { type: 'string', description: 'Concrete inputs or league state that give a wrong result' },
          suggested_fix: { type: 'string' },
        },
        required: ['severity', 'where', 'problem', 'failure_scenario', 'suggested_fix'],
      },
    },
    checked: { type: 'string', description: 'What you actually read and ran' },
  },
  required: ['verdict', 'findings', 'checked'],
}

const FIX_SCHEMA = {
  type: 'object',
  properties: {
    final_status: { type: 'string', enum: ['fixed', 'partly_fixed', 'reverted', 'not_a_real_gap', 'blocked'] },
    findings_applied: { type: 'string' },
    findings_rejected: { type: 'string', description: 'Each rejected finding and why it was wrong' },
    test_result: { type: 'string', description: 'cargo test --workspace: number of passing test binaries, any failures verbatim' },
    real_data_effect: { type: 'string', description: 'Final before/after on the real leagues after the fixes' },
    docs_updated: { type: 'string' },
  },
  required: ['final_status', 'findings_applied', 'findings_rejected', 'test_result', 'real_data_effect', 'docs_updated'],
}

const UNITS = [
  {
    key: 'A2-season-length',
    task: `Gap A2 in the gap analysis: constants assume an 82-game season but the loaded 2026-27 schedule has 84 games per team. Take games per team from the loaded schedule wherever a projected goalie start share or a no-schedule fallback divides by a hard-coded 82 (valuate.rs PRIOR_TEAM_GAMES and its uses, adjust.rs, base.rs, harness outcomes.rs), keeping 82 only as the fallback when no schedule is loaded and for genuinely prior-season quantities (last season really was 82 games: check each use and decide which season it describes). IMPORTANT: an earlier agent started this unit and was stopped part-way, so the tree holds its unfinished, unreviewed edits in crates\fm-engine\src\valuation\valuate.rs, adjust.rs and schedule.rs, crates\fm-engine\src\recommend\base.rs and injuries.rs, and the tests crates\fm-engine\tests\valuation_base.rs and recommend_base.rs. Start by reading "git diff -- crates/fm-engine" in full and running "cargo check --workspace --tests"; the tree may not compile. Treat those edits as a draft: keep what is correct, fix or remove (by editing by hand) what is not, and finish the unit. The uncommitted changes in crates\fm-harness\src\deployment.rs, crates\fm-providers, crates\fm-sources, crates\fm-web and crates\fm-core\src\config.rs are earlier finished work by the owner's session and must be left as they are.`,
  },
  {
    key: 'A1-harness-fantrax-daily-lock',
    task: `Gap A1: the harness matches and grades Fantrax lineup advice as if lineups locked weekly on Monday (crates\\fm-harness\\src\\episode_match.rs first_lockable_day and the lineup matching window, crates\\fm-harness\\src\\outcomes.rs grading window), but this Fantrax league changes lineups daily and the engine already reads that (crates\\fm-engine\\src\\recommend\\lineup_lock.rs). Pass the league's real lock mode into the matcher and grader so a daily-lock Fantrax league is handled like the ESPN daily path, and a weekly-lock Fantrax league keeps today's behaviour. Do not rewrite stored harness data; describe in your report whether already-graded Fantrax lineup rows are now stale and how the owner could regrade them with existing commands.`,
  },
  {
    key: 'B2-injured-player-value',
    task: `Gap B2: an injured player's season value is fpg times availability, as if his roster slot produced nothing while he is out (valuate.rs around the availability multiplier and the VORP subtraction). When the player is in, or can be moved to, an IR slot, the slot is refilled, so his season value should be availability x FPG + (1 - availability) x replacement level at his position; when no IR slot is available to that team the current number is right. Reconcile this with the hold-or-cut logic in crates\\fm-engine\\src\\recommend\\injuries.rs so the two parts of the program agree about the same player, and check the waiver engine's injured-drop handling (base.rs, waivers.rs) still makes sense and does not double count. Verify on real data with Brad Marchand and Brayden Point in the ESPN league (the analysis gives their before numbers) and with injured goalies.`,
  },
  {
    key: 'B5-goalie-lineup-value',
    task: `Gap B5: the daily lineup solver values a goalie's day as per-start FPG times start probability, floored at zero, with no opponent term (crates\\fm-engine\\src\\recommend\\lineup.rs around the day value), while a separate matchup-aware goalie planner exists (crates\\fm-engine\\src\\analysis\\goalie_plan.rs) and is not used by the lineup. Step 1 (required): use the goalie planner's expected points if he starts in the lineup's day value, and remove the zero floor so a start with negative expected points is benched when the scoring makes that possible (ESPN: GA -1.5, SV 0.2, W 2; break-even save percentage about .882. Fantrax break-even is about .800, so almost every start stays positive there). Step 2 (do it if it fits cleanly, otherwise report it as left undone with a concrete design): on the final day or two of a matchup, when the matchup win probability is available, choose whether to start each goalie by comparing win probability with and without the start rather than expected points. Keep the lineup deterministic for a given input.`,
  },
  {
    key: 'B3-scaled-thresholds-and-ranges',
    task: `Gap B3: recommendation thresholds are fixed fantasy-point amounts shared by both leagues (waiver bars in crates\\fm-engine\\src\\recommend\\base.rs, lineup swap bar in lineup.rs, trade proposal bar and offer verdict edges in recommend\\trades.rs, planned-move bars in recommend\\plan.rs), although a point is worth far less in the Fantrax scoring (replacement forward about 4.45 FPG) than in ESPN (about 1.60). Express every such threshold as a multiple of a per-league unit computed from the league's own data so that the ESPN behaviour stays numerically where it is today (ESPN is the league the constants were tuned on) and Fantrax scales with its scoring. Choose the unit carefully and justify it (candidates: the replacement-level forward's FPG, or the spread of FPG among rostered players) and apply the same unit everywhere. Then attach a per-position projection error to player values using the repo's own backtest errors (crates\\fm-engine\\data\\fitted_params.json mae_at_fitted_k_multi, scaled to the league unit) and report a trade's gain as a range; the verdict must be "too close to call" (the existing counter / too-close band) when the range includes zero. Goalies get the wider error the backtest shows.`,
  },
  {
    key: 'B1-started-games-value',
    task: `Gap B1 (the highest-impact gap): rest-of-season value counts only the best starting lineup, so bench players, a third goalie and the schedule are worth zero in trades (crates\\fm-engine\\src\\recommend\\trades\\engine.rs lineup gain), in season-horizon waiver gains (recommend\\waivers.rs) and in the outlook's team strength (analysis\\outlook.rs). The this-matchup waiver path already values a pickup by the lineup games it adds (waivers.rs, the matchup path), so the method exists. Implement the first step the analysis recommends: a rest-of-season roster value equal to the sum over players of per-game rate times the fraction of his team's games he would actually be started, where the start fraction comes from solving the daily lineup over the next four weeks of the real schedule for that roster (reuse the existing daily lineup/plan machinery, cache per roster so trade search stays fast, and measure the run time of "fm trades" and "fm trade-eval" before and after). Use it in the trade evaluator and trade search, in season-horizon waiver gains, and in the outlook's team strength. A goalie's started games must respect his start share and the two goalie slots. Verify with the analysis's example: "fm trade-eval -l espn --give \\"Brandon Bussi\\" --get \\"Jake DeBrusk\\"" must no longer value the third goalie at zero. Also add the regression test the analysis calls for in D7: a goalie-plus-winger-for-one-centre trade fixture. If the full wiring into all three consumers is too large to finish safely, finish the trade evaluator and trade search first and report the rest as left undone with a precise plan.`,
  },
  {
    key: 'B7-B9-acceptance-and-matchup-spread',
    task: `Gaps B7 and B9. B7: the trade acceptance model puts an even-by-market offer at 50% (crates\\fm-engine\\src\\recommend\\trades.rs and trades\\accept.rs), but the research in ${KB}\\06-trade-strategy.md says owners value their own players above market (the endowment effect), so an even offer is declined more often than accepted; and a calibration function exists in accept.rs that nothing calls. Shift the default so an even offer sits below 50% with a basis you can cite from file 06 (the analysis suggests 35 to 40% until data exists; check what the sources support), and wire the existing calibration into the harness refit path so it replaces the default once enough real trade outcomes are recorded (find how the harness records proposed and accepted trades; if it does not record what the calibration needs, report exactly what is missing instead of guessing). Also make trade proposals consider the other side's gain by our own model, which the analysis says is filtered the wrong way round (trades\\engine.rs around the proposal filter): verify that claim first. B9: the matchup simulation's per-game spread (crates\\fm-engine\\src\\analysis\\matchup.rs) grows with the square root of FPG, so it cannot be calibrated for both scoring systems at once, and the harness already fits a per-league scale that is never applied (crates\\fm-harness matchup_calib). Apply the fitted per-league scale when enough graded matchups exist (the harness docs say eight), and make the default spread scale correctly with the league's scoring so doubling every scoring weight doubles the spread.`,
  },
  {
    key: 'C2-outlook-luck-and-small-items',
    task: `Gap C2 and the "Smaller items". C2: add to "fm outlook" (crates\\fm-engine\\src\\analysis\\outlook.rs and its CLI/web rendering) each team's points-for rank and all-play record (how a team would have done playing every other team each completed week; use the weekly scores the providers already have, and say in your report if they are not available for one league), so a bad record can be told apart from a weak roster; break playoff-simulation ties on points for, which is ESPN's actual second tiebreaker per ${KB}\\08-h2h-objectives-and-game-theory.md, instead of projected strength; and report how much the owner's playoff odds change with a win versus a loss this week. Smaller items: (1) the last-move reserve bar should not apply on the final day of a matchup period, when an unused move is worth nothing (crates\\fm-engine\\src\\recommend\\base.rs around the final-move bar): verify and fix. (2) Investigate the suspected problem that the positional mean used as the shrink target depends on how many free agents each provider loaded (valuate.rs positional means; ESPN loads about 326 free agents, Fantrax about 738): measure how different the means are from a fixed population (for example players rostered in the league, or the top N by games played) and fix it only if the effect on values is material; report the measurement either way.`,
  },
  {
    key: 'KB-corrections',
    model: 'sonnet',
    task: `Apply the "Corrections to the knowledge files" section of ${KB}\\09-engine-gap-analysis.md to files 01 to 08 (edit the knowledge files, not code), using the confirmed league facts in ${KB}\\README.md: Fantrax lineups change daily; the Fantrax roster is capped at 16 so a full 12-player active lineup leaves four reserve spots; ESPN allows 2 adds per matchup period; FT is fights, not a faceoff statistic; each Fantrax team keeps its top 3 players, so file 07's full-dynasty reasoning applies to the three keeper slots; 8 of 14 ESPN teams and 6 of 12 Fantrax teams make the playoffs with 18 regular-season weeks left on 2026-11-10; the ESPN goalie scoring is W 2, GA -1.5, SO 2, OTL 1, SV 0.2, not ESPN's defaults. Fix each wrong statement in place, keep the sources and structure, and where a file's conclusion changes because of a corrected fact, rewrite the conclusion. Then update ${KB}\\09-engine-gap-analysis.md itself: read the reports of the earlier units below and, for each gap they addressed, add a "Status" line under the gap's heading saying what was changed and what remains, and bring the "Open league facts still unknown" table and "Suggested order of work" up to date. This unit changes no code, so there is nothing to compile; still run "cargo test -p fm-web --test assets" to confirm nothing else was disturbed.`,
  },
]

const results = []

for (const u of UNITS) {
  const earlier = results.map(r => ({ unit: r.key, status: r.final ? r.final.final_status : (r.impl ? r.impl.status : 'no result'), summary: r.impl ? r.impl.summary : '', files: r.impl ? r.impl.files_changed.map(f => f.file) : [] }))

  const impl = await agent(`${RULES}

YOUR UNIT: ${u.key}
${u.task}

HOW TO WORK
1. Read ${KB}\\README.md, then the entry for this unit in ${KB}\\09-engine-gap-analysis.md, then the "Implications for the engine" sections it links to in files 01 to 08.
2. Before changing anything, verify the gap analysis's claims in the code (it was written by a reader, not the author, and may be wrong). If the code already does the right thing, change nothing and report status "not_a_real_gap" with the evidence.
3. Capture a BEFORE picture on real data: make sure ${REPO}\\target\\debug\\fm.exe is built from the current tree ("cargo build -p fm-cli"), then save the outputs you will compare to ${SCRATCH}\\${u.key}\\before (create the folder), for example "fm.exe roster --all-teams --json -l espn", the same for fantrax, and whichever of trade-eval, waivers, lineup, outlook, matchup or plan (all with --json) this unit affects. Some commands are slow (several minutes); that is expected.
4. Implement the fix under the hard rules. Add unit tests that would have failed before the change and that pin the new behaviour with hand-checkable numbers.
5. Run "cargo test --workspace" until everything passes, rebuild the debug binary, capture the AFTER picture in ${SCRATCH}\\${u.key}\\after, and compare. Check at least three changed values by hand against the formula you implemented. If a result looks wrong, find out why before reporting.
6. Report honestly. If tests fail and you cannot fix them, say so and leave the tree compiling. If you could only do part of the unit, say exactly what is left.

EARLIER UNITS IN THIS RUN (already in the tree; build on them, do not undo them):
${JSON.stringify(earlier)}`, { label: `implement:${u.key}`, phase: 'Implement', schema: IMPLEMENT_SCHEMA, agentType: 'general-purpose', model: u.model || 'opus' })

  if (!impl) {
    log(`${u.key}: implementer returned nothing; moving on`)
    results.push({ key: u.key, impl: null, reviews: [], final: null })
    continue
  }
  log(`${u.key}: implementer says ${impl.status}`)

  if (impl.status === 'not_a_real_gap' || impl.status === 'blocked' || impl.files_changed.length === 0) {
    results.push({ key: u.key, impl, reviews: [], final: null })
    continue
  }

  const reviewBase = `${RULES}

You are REVIEWING a change another agent just made for unit ${u.key}. You must not edit any file and must not run cargo build or cargo test (another reviewer is reading the same tree and a later step runs the tests). You may run the already-built debug binary ${REPO}\\target\\debug\\fm.exe with read-only commands, read-only git commands such as "git diff -- <file>", and read any file. The implementer's before/after captures are in ${SCRATCH}\\${u.key}.

THE UNIT:
${u.task}

THE IMPLEMENTER'S REPORT:
${JSON.stringify(impl)}

Read every function the report lists, in full, in the current tree, plus the gap entry in ${KB}\\09-engine-gap-analysis.md. Report only problems you can tie to a concrete failure scenario. Do not report style preferences. If the change is sound, say so with an empty findings list.`

  const reviews = (await parallel([
    () => agent(`${reviewBase}

YOUR LENS: correctness and regressions. Look for logic errors, wrong units (per game versus per team game versus per week), double counting with existing adjustments, off-by-one errors in dates and periods, division by zero and empty-roster cases, behaviour for the other league and for a league with no schedule or no IR slots loaded, values that can go negative or exceed sensible bounds, non-determinism, performance traps (work repeated inside a search loop), tests that assert too little or were loosened to pass, and existing behaviour that was changed without being mentioned in the report.`, { label: `review-correctness:${u.key}`, phase: 'Review', schema: REVIEW_SCHEMA, agentType: 'general-purpose', model: 'sonnet' }),
    () => agent(`${reviewBase}

YOUR LENS: does it achieve what the research asks, on real data. Read the research sections the gap entry cites. Check that the change implements the idea rather than a weaker look-alike, that every new constant has a real basis (a cited source, a league setting, or the repo's own fitted numbers) and is not invented, and that the effect on the two real leagues is in the right direction and of a believable size. Use the before/after captures and run read-only fm commands yourself to spot-check at least five players or recommendations, including the examples named in the gap entry. Flag any player, verdict or recommendation that the change makes clearly worse.`, { label: `review-intent:${u.key}`, phase: 'Review', schema: REVIEW_SCHEMA, agentType: 'general-purpose', model: 'sonnet' }),
  ])).filter(Boolean)

  const findings = reviews.flatMap(r => r.findings)
  log(`${u.key}: ${reviews.length} reviews, ${findings.length} findings (${findings.filter(f => f.severity === 'blocker').length} blockers)`)

  const final = await agent(`${RULES}

YOUR JOB: finish unit ${u.key} after review. You may edit code and run cargo.

THE UNIT:
${u.task}

THE IMPLEMENTER'S REPORT:
${JSON.stringify(impl)}

REVIEW RESULTS (two independent reviewers):
${JSON.stringify(reviews)}

1. For each finding, check it against the code yourself. Apply the ones that are real, starting with blockers. Reject, with a reason, any that are wrong. If the reviewers judged the change wrong and you agree after checking, undo that change by editing the files back by hand (use "git diff -- <file>" to see what changed; never use git checkout, restore, stash or reset) and report final_status "reverted".
2. Run "cargo test --workspace". Every test binary must pass. Fix what you broke. Then "cargo build -p fm-cli".
3. Re-run the real-data comparison (${SCRATCH}\\${u.key}\\before versus fresh output from ${REPO}\\target\\debug\\fm.exe) and report the final effect.
4. Unless this unit is the knowledge-corrections unit, add a "- **Status (2026-11-10):** ..." line under this gap's heading in ${KB}\\09-engine-gap-analysis.md saying in one or two sentences what was changed, where, and what remains. If the change is user-visible (a new setting, a changed command output), update README.md or the relevant file in ${REPO}\\docs in the place that already describes it.
5. Leave the tree compiling with all tests passing.`, { label: `fix-and-gate:${u.key}`, phase: 'Fix and gate', schema: FIX_SCHEMA, agentType: 'general-purpose', model: u.model || 'opus' })

  log(`${u.key}: final ${final ? final.final_status : 'no result'}`)
  results.push({ key: u.key, impl, reviews, final })
}

return results.map(r => ({
  unit: r.key,
  implementer_status: r.impl ? r.impl.status : null,
  final_status: r.final ? r.final.final_status : null,
  summary: r.impl ? r.impl.summary : null,
  claim_check: r.impl ? r.impl.claim_check : null,
  files_changed: r.impl ? r.impl.files_changed : [],
  constants: r.impl ? r.impl.constants_introduced : null,
  left_undone: r.impl ? r.impl.left_undone : null,
  review_verdicts: r.reviews.map(x => x.verdict),
  findings: r.reviews.flatMap(x => x.findings).map(f => `${f.severity}: ${f.where}: ${f.problem}`),
  findings_applied: r.final ? r.final.findings_applied : null,
  findings_rejected: r.final ? r.final.findings_rejected : null,
  test_result: r.final ? r.final.test_result : (r.impl ? r.impl.tests : null),
  real_data_effect: r.final ? r.final.real_data_effect : (r.impl ? r.impl.real_data_effect : null),
}))