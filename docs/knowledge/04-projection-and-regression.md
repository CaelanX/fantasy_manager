# 04 - Projection and regression: what predicts future hockey production

Researched 2026-11-10. Every factual claim links to the page it was read on. Claims that were seen only in a search excerpt, and not on the page itself, are marked "(unverified)".

## What this covers

This note is about the statistics of projecting hockey players: which numbers carry real information about the future, how much each one should be pulled back toward average ("regression to the mean"), how expected goals and goalie metrics fit in, how published projection systems are built, what aging curves say, how fast a projection should move during a season, and how to test a projection system honestly. It ends by comparing that evidence with how our engine works today (the description in the task brief plus `docs/backtesting.md`).

## Key takeaways

- The standard way to combine evidence is "observed rate plus k units of league average". The weight on the observed rate is n / (n + k), where n is the sample and k is the sample size at which the stat is half signal, half noise. Tom Tango's Marcel does exactly this with 1,200 plate appearances of league average ([Tango](https://www.tangotiger.net/archives/stud0346.shtml)).
- Volume stats (shots, and by extension hits and blocks) are far more repeatable than percentage stats (shooting percentage, on-ice shooting percentage, save percentage). Percentages need very large samples.
- Published constants: individual shooting percentage is regressed with about 375 shots of league average for forwards and 275 for defensemen ([Sprigings and Toumi](https://hockey-graphs.com/2015/10/01/expected-goals-are-a-better-predictor-of-future-scoring-than-corsi-goals/)); goalie save percentage needs about 3,000 shots to be half talent ([Paine](https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie)); team PDO after 1,000 shots regresses 87% ([Desjardins](https://www.arcticicehockey.com/pdo-regression-to-the-mean-or-why-you-should-ignore-shooting-1550520512/)).
- Expected goals (xG) beat raw goals and raw shooting percentage as a predictor of an individual's future goals, but at team level xG, shot attempts (Corsi) and scoring chances predict future goals about equally well (r-squared 0.22 to 0.25) ([Leafs Nation primer](https://theleafsnation.com/news/here-we-go-again-advanced-stats-and-analytics-primer-part-1)).
- Finishing talent (reliably scoring more than xG) is real but small and concentrated in a few elite shooters; most players sit near zero ([PPP](https://pensionplanpuppets.com/sick-hands-shooting-talent-in-the-nhl), [Noel 2025](https://arxiv.org/html/2511.07703v2)).
- Goalies are the least predictable position. Year-to-year correlations of 0.12 (goals saved above expected and save percentage, [JFresh](https://jfresh.substack.com/p/why-goaltending-is-basically-random?s=r)) to about 0.30 (adjusted goals-against, [Paine](https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie)) are reported. Goalie fantasy value should lean on volume (starts, shots faced, team wins) more than on rate skill.
- Secondary assists are close to noise for defensemen (year-to-year correlation 0.03) and weak for forwards (0.19); primary assists are better (0.36 forwards, 0.20 defensemen) ([Fooled by Grittiness](https://fooledbygrittiness.blogspot.com/2016/04/to-secondary-assist-or-not-to-secondary_15.html)).
- Aging studies disagree on the peak: 22 to 25 for skater value by the delta method ([Evolving Wild](https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/)), about 26 for forwards in a newer delta-method application ([Schuckers, Lopez and Macdonald](https://ar5iv.arxiv.org/html/2110.14017)), and 27 to 29 for scoring in a regression study ([Brander, Egan and Yeung](https://ideas.repec.org/a/bpj/jqsprt/v10y2014i2p19n6.html)). The method drives the answer.
- There is no published evidence for a "hot hand" in hockey; the two direct studies (shooters, playoff goalies) found a slightly negative effect ([Chachad et al. review](https://pmc.ncbi.nlm.nih.gov/articles/PMC10800907), [Ding et al.](https://arxiv.org/abs/2102.09689)). Our backtest finding that recent form barely predicts is consistent with this.
- A role change (ice time, power-play unit) is a change in the thing being measured, not noise, and should move the projection immediately; a results change with unchanged role should move it slowly.
- Published hockey fantasy projection systems rarely report accuracy. Our own rolling-origin backtest against "last season" and Marcel baselines is already more rigorous than most of what is published; the main risk is overfitting the fitted constants.

## 1. Regression to the mean and stabilisation

**Regression to the mean** means that a player whose observed number is far from average is, on average, closer to average in the next sample, because part of the gap was luck. Garret Hohl stresses that it "doesn't imply that bad luck predicts future good luck"; the expectation simply moves toward the mean, and players who outperformed still tend to outperform, but "about 80% less than before, on average" ([Hohl 2025](https://thefivehohl.substack.com/p/what-does-regression-to-the-mean)).

**Stabilisation point** is the sample size at which half of the observed spread between players is talent and half is chance. In the "add k of league average" formula it is the constant k: with n = k observations the estimate sits halfway between the observed rate and the mean. Tango's Marcel is the canonical example: seasons weighted 5/4/3, plus league-average performance "at a total of 1200 PA for each player (2 weights x 600 PA)" ([Tango](https://www.tangotiger.net/archives/stud0346.shtml)).

Published hockey numbers:

| Stat | Published finding | Source |
|---|---|---|
| Individual shooting percentage | Regression constant about 375 shots (forwards), 275 shots (defensemen) | [Sprigings and Toumi 2015](https://hockey-graphs.com/2015/10/01/expected-goals-are-a-better-predictor-of-future-scoring-than-corsi-goals/) |
| Team shooting percentage (odd/even games, road even strength) | Split-half slope 0.228, r-squared 0.0478; "luck is 85% of the observed range"; shot quality "just under 10%" | [Desjardins](https://www.arcticicehockey.com/luck-vs-shot-quality-in-shooting-percentage-1550491415/) |
| Team shot distance / location (same test) | r-squared 0.51 / 0.45, far more repeatable than the percentage | [Desjardins](https://www.arcticicehockey.com/luck-vs-shot-quality-in-shooting-percentage-1550491415/) |
| PDO (team shooting percentage plus save percentage) | "PDO through the first 1000 shots regresses 87% to the mean over the rest of the season"; 1030 becomes about 1004 | [Desjardins](https://www.arcticicehockey.com/pdo-regression-to-the-mean-or-why-you-should-ignore-shooting-1550520512/) |
| On-ice shooting percentage (team shooting percentage while a skater is on the ice) | Of 138 skater-seasons above 11% in ten years, "only five had a similarly high oiSh% the next season. That's 3.6%" | [JFresh 2020](https://jfresh.substack.com/p/percentage-luck-in-hockey-explained) |
| On-ice shooting percentage | Regress about 67% toward league average over three-year windows (Tulsky, reported by TSN); past rate "plus 657 shots at a league average" | (unverified: search excerpts only; the TSN page returned 404 and the forum page 403) |
| Primary assists per 60 (A1) | Year-to-year correlation 0.36 forwards, 0.20 defensemen (2010-16, 400+ minutes) | [Fooled by Grittiness](https://fooledbygrittiness.blogspot.com/2016/04/to-secondary-assist-or-not-to-secondary_15.html) |
| Secondary assists per 60 (A2) | 0.19 forwards, 0.03 defensemen ("almost virtually random") | same |
| Total assists per 60 | 0.43 forwards, 0.24 defensemen | same |
| Primary shot contributions (own shots plus passes leading to shots) | Best prediction of rest-of-season primary points reached after "only eleven games of data"; primary assists per 60 peaked at 0.261 after 8 games | [Stimson 2016](https://hockey-graphs.com/2016/01/27/redefining-shot-quality-one-pass-at-a-time/) |
| Power-play results | With about 40-game samples the best predictor of power-play success was unblocked shot attempts (Fenwick) per 60, not goals (Purdy; supported by Desjardins and Tulsky) | [Parnass 2016](https://hockey-graphs.com/2016/01/14/special-teams-analytics-in-the-21st-century/) |
| Hits (possession-independent) | Year-to-year r-squared 0.67 (2011-12 to 2012-13) | [Parnass 2014](https://www.habseyesontheprize.com/introducing-possession-independent-hitting-and-shot-blocking/) |
| Blocked shots (possession-independent) | Year-to-year r-squared 0.37 | same |
| Goalie save percentage | "only begins to stabilize after facing around 3,000 shots"; year-to-year correlation 0.296 for adjusted goals-against; "only 30 percent of the difference ... 'belongs' to the goalie" | [Paine 2014](https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie) |
| Goalie save percentage | Pad each goalie's weighted record to 4,000 shots with league-average shots (author calls the figure "kind of arbitrary") | [garik16 2014](https://hockey-graphs.com/2014/02/03/forecasting-future-goalie-performance-with-four-year-hockey-marcels/) |
| Goalie even-strength save percentage | About 1,500 shots before results are near skill level (Desjardins, as cited) | [Hohl 2014](https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/) |
| Goals saved above expected (GSAx) and save percentage | Year-to-year r = 0.12 for both (18 three-year starters); regular season to playoffs r = 0.15 | [JFresh 2020](https://jfresh.substack.com/p/why-goaltending-is-basically-random?s=r) |

Notes on the gaps. **Individual points percentage (IPP)** is the share of on-ice goals on which a player gets a point. I found no primary source with a quantified repeatability figure; fantasy writers treat a large one-season move as likely to revert (unverified, search excerpts from DobberHockey only). **Goalie win rate** has no published stabilisation number that I could find; wins depend on team goal support, and Evolving-Hockey projects wins from raw volume because rates are unreliable ([Evolving-Hockey 2021](https://evolving-hockey.com/blog/2021-fantasy-hockey-projections/)). **Individual shot rate and individual xG**: I found no clean published year-to-year correlation, only the indirect evidence above that shot volume and location are much more repeatable than conversion. The hits and blocks figures are for possession-adjusted versions from a single pair of seasons; raw counts also carry scorekeeper bias that differs by arena (unverified here: [Schuckers and Macdonald, arXiv 1412.1035](https://arxiv.org/pdf/1412.1035) appeared in search results but was not read).

## 2. Expected goals

**Expected goals (xG)** assigns each unblocked shot attempt a probability of becoming a goal from its circumstances, then sums those probabilities. **Corsi** counts all shot attempts; **Fenwick** counts unblocked attempts. Public models use shot distance and angle, shot type, the previous event and time since it, manpower, score state and similar play-by-play features ([Evolving-Hockey](https://evolving-hockey.com/?p=394), [MoneyPuck](https://moneypuck.com/about.htm)). Reported discrimination (AUC, where 0.5 is a coin flip and 1.0 is perfect) is about 0.78 at even strength for Evolving-Hockey (0.7747 on a held-out 2017-18 season) and 0.807 overall for MoneyPuck's rebuilt model.

How predictive is it?

- **Individual level.** Sprigings and Toumi reported that "individual xG per 60 minutes (ixG/60) outperforms iCF% and Sh% across the board" for predicting future goals, and that at player on-ice level xG outperformed earlier measures "as early as the 10-game mark" ([Hockey Graphs](https://hockey-graphs.com/2015/10/01/expected-goals-are-a-better-predictor-of-future-scoring-than-corsi-goals/)). Their numbers are in image tables and were tested only by the authors.
- **Team level.** The same authors said xG matched score-adjusted Corsi and goals at 20 games and was "far more superior" after that. An independent comparison across three public data sites since 2013 found almost no difference: r-squared with future goals of 0.24 (MoneyPuck xG), 0.22 (Evolving-Hockey xG), 0.23 to 0.24 (Corsi) and 0.25 (scoring chances) ([Leafs Nation](https://theleafsnation.com/news/here-we-go-again-advanced-stats-and-analytics-primer-part-1)). This is a genuine disagreement.
- **Flurries.** MoneyPuck discounts rebound sequences ("flurry adjusted" xG) and says the adjusted figure is "more repeatable and also more predictive of future winning", without publishing the test ([MoneyPuck](https://moneypuck.com/about.htm)).

Known limits: public models have no passing data, so cross-ice passes are undervalued ([Evolving-Hockey](https://evolving-hockey.com/?p=394)); screens and the positions of other players are unseen ([Noel 2025](https://arxiv.org/html/2511.07703v2)); and Evolving-Hockey validated its model only on classifying shots as goals, stating that it had not run a future-scoring test of the Sprigings and Toumi kind ([Evolving-Hockey](https://evolving-hockey.com/?p=394)).

**Finishing talent** is the ability to score more than xG. The evidence says it exists but is small and hard to detect:

- Evolving-Hockey offered shooter talent to its gradient-boosted model and "this variable (in each model) was never used in any decision tree" ([Evolving-Hockey](https://evolving-hockey.com/?p=394)).
- Sprigings and Toumi included a regressed "shot multiplier"; an author comment recalled that without it xG was "about on par with Corsi" (informal, [Hockey Graphs](https://hockey-graphs.com/2015/10/01/expected-goals-are-a-better-predictor-of-future-scoring-than-corsi-goals/)).
- A 2025 skill-adjusted model improved log loss and AUC by "in the range of 1-5%", with the largest gain (AUC 0.7238 to 0.7616) on shots by the most skilled shooters and little effect elsewhere; the author calls the gains "not monumental" ([Noel 2025](https://arxiv.org/html/2511.07703v2)).
- Across 6,151 forward seasons since 2008 the mean goals-minus-xG is "just a hair over zero", yet Auston Matthews is +89 goals for his career; 369 forwards have had a 20-goal season at 110% of xG, and 156 of them did it only once ([PPP 2024](https://pensionplanpuppets.com/sick-hands-shooting-talent-in-the-nhl)).
- Micah McCurdy's Magnus model estimates shooter and goalie effects with a heavy ridge penalty toward zero and finds individual effects smaller than the effect of a rush shot ([HockeyViz](https://www.hockeyviz.com/txt/magnusSG)).

The practical reading: regress goals toward xG, but toward the player's own long-run finishing ratio (itself heavily regressed), not toward exactly 1.0.

## 3. Goalies

Save percentage is noisy because the gap between goalies is small relative to shot-level randomness. Paine puts the half-talent point at about 3,000 shots, roughly a season and a half for a workhorse, with only about 30% of a single season's gap from average belonging to the goalie ([Paine](https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie)). JFresh found year-to-year r = 0.12 for both GSAx (save performance relative to the xG of shots faced) and plain save percentage, and that of nine league-best goalie seasons, two stayed elite and five finished below average the next year ([JFresh](https://jfresh.substack.com/p/why-goaltending-is-basically-random?s=r)). Evolving-Hockey calls save percentage "one of the most unreliable and unrepeatable metrics" and projects goalies from starts, shots against, goals against and wins instead ([Evolving-Hockey 2021](https://evolving-hockey.com/blog/2021-fantasy-hockey-projections/)).

Against that, Hohl (citing Tulsky) argues save percentage is "highly repeatable and predictive, as far as sports statistics go, once sufficient sample size is met", and that prior save percentage predicted current results about three times better than salary ([Hohl 2014](https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/)). The two views are compatible: multi-year samples carry signal, single seasons carry little.

**What GSAx adds.** It removes shot-quality differences that team defence creates. JFresh expected save percentage to look more repeatable "because of the team defence factor" (a goalie stays behind the same defence), yet both came out at 0.12. So GSAx is a cleaner description of what the goalie did, but there is no published evidence that it predicts future results better than multi-year save percentage. MoneyPuck's game model uses both, over several seasons weighted to recent play, and gives goaltending 29% of the weight in predicting a game ([MoneyPuck](https://moneypuck.com/about.htm)).

**Goalie Marcel.** garik16 weighted four seasons of shots 5/3/2.5/1.5 (most recent first), padded to 4,000 shots with league average, applied no aging, and reported no backtest ([Hockey Graphs](https://hockey-graphs.com/2014/02/03/forecasting-future-goalie-performance-with-four-year-hockey-marcels/)).

## 4. Projection systems

| System | Season weighting | Regression | Aging | Rookies | Accuracy reported |
|---|---|---|---|---|---|
| Marcel (baseball) | 5/4/3 | 1,200 PA of league average | Rates: +0.6% per year under 29, -0.3% per year over | Not covered on the page read | None on the page read ([Tango](https://www.tangotiger.net/archives/stud0346.shtml)) |
| Goalie Marcel | 5/3/2.5/1.5 on shots | Pad to 4,000 shots | None | n/a | None ([garik16](https://hockey-graphs.com/2014/02/03/forecasting-future-goalie-performance-with-four-year-hockey-marcels/)) |
| Evolving-Hockey fantasy | Gradient-boosted trees on 2-3 prior seasons; prior-year values of each stat were the strongest predictors | Implicit; outputs were so compressed that they were rescaled to the observed mean and spread (McDavid 97 to 116 points) | Age is a model input | One-year players get "modifications" | None published ([Evolving-Hockey](https://evolving-hockey.com/blog/2021-fantasy-hockey-projections/)) |
| MoneyPuck game model | Several seasons, weighted to recent | Bayesian shooting-talent and goalie-talent estimates | n/a | n/a | Favourite wins 60 to 64% of games; log loss 0.648 to 0.661 by season ([MoneyPuck](https://moneypuck.com/about.htm)) |
| HockeyViz Magnus (shooting) | Pools three seasons | Ridge penalty toward league average, lambda = 100 for players | Not described on this page | n/a | None on this page ([HockeyViz](https://www.hockeyviz.com/txt/magnusSG)) |

Three points from Evolving-Hockey are directly relevant to a fantasy engine: advanced value metrics (GAR, xGAR, RAPM) "were weak at predicting box-score stats"; modelling raw counts versus rates gave "extremely similar" results; and game-winning goals, shutouts and overtime losses were not projected because they are "quite random". Its rest-of-season number uses no new model: it is the preseason per-game rate times estimated remaining games, added to actual totals ([user guide](https://evolving-hockey.com/?p=692)).

Evolving-Hockey's three-year weights for its value metrics are 49% / 32% / 19% most recent first (unverified: search excerpt of its player-card notes). That is steeper than Marcel's 5/4/3 (42% / 33% / 25%).

I could not retrieve a methodology page for Dom Luszczyszyn's GSVA / Net Rating (The Athletic is paywalled), DobberHockey or Apples & Ginos. What is verified: the Apples & Ginos author says projections rest on assumptions about ice time, linemates and power-play usage, and should be read as a starting point ([Yahoo 2022](https://sports.yahoo.com/nhl-fantasy-hockey-how-to-make-sense-of-player-projections-165941054.html)). DobberHockey's Michael Clifford published a rare self-audit: his 2024-25 forward projections overshot goals by 11.4% and points by 6.5%, mostly because league power-play opportunities fell to a record low and projected shots (2.17 per game) exceeded actual (1.99); removing the power-play gap cut the points miss to 2.9% ([Clifford 2025](https://dobberhockey.com/2025/05/23/ramblings-how-shots-power-play-opportunities-and-goal-distribution-affected-preseason-projections-may-23)). The lesson is that league-environment drift (power-play frequency, scoring level) is a shared error across all players that a player-by-player model does not see.

**Rookies and NHL equivalencies (NHLe).** NHLe converts a point in another league into NHL points using players who moved between leagues. One network-based version gives KHL 0.772, SHL 0.566, Liiga 0.441, AHL 0.389, NCAA 0.194, OHL 0.144, WHL 0.141, QMJHL 0.113, with a cross-validated mean absolute error of 0.33 points per game ([hockeystats.com](https://hockeystats.com/methodology/nhle)). Older factor tables differ (for example AHL near 0.47; unverified, search excerpt). Stated weaknesses: classic NHLe bakes in a year of development, ignores team quality, position and age, and uses only players who made the jump ([Wu and Gall 2020](https://hockey-graphs.com/2020/03/02/which-league-is-best/)). Goalie translation is weaker still, with some league links resting on 3 to 5 goalies ([Chatel 2023](https://thibaudchatel.substack.com/p/goalies-nhle-and-league-equivalents)).

## 5. Aging curves

The **delta method** averages each player's year-over-year change at each age and chains the averages into a curve. It only sees players who played in both years, and players who decline sharply often lose their jobs, so it is exposed to **survivorship bias**.

| Study | Measure | Finding |
|---|---|---|
| [Evolving Wild 2017](https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/) | WAR, delta method, 2008-16 | Skaters peak about 23, better described as a plateau from 22 to 25; "defensemen basically plateau from ages 19 to 24 and decline steadily after that"; no survivorship correction |
| [Schuckers, Lopez and Macdonald](https://ar5iv.arxiv.org/html/2110.14017) | Forwards (2,276 players) | Delta method tends "to peak around age 26"; spline models with imputation for missing seasons had the lowest error in simulation |
| [Brander, Egan and Yeung 2014](https://ideas.repec.org/a/bpj/jqsprt/v10y2014i2p19n6.html) | Scoring and plus-minus | "scoring peak age is between 27 and 28 for forwards and between 28 and 29 for defencemen"; near-peak from about 24 to 32 (forwards) and 24 to 34 (defencemen) |
| [Giampietro 2025](https://thehockeywriters.substack.com/p/how-does-the-production-of-nhl-forwards-change-with-age) | Per-60 rates, 6,000+ forward seasons | Peak in mid-twenties; goals per 60 decline most clearly; primary assists hold until "a few years into their thirties" |
| [garik16 2014](https://hockey-graphs.com/2014/03/21/how-well-do-goalies-age-a-look-at-a-goalie-aging-curve/) | Goalie save percentage vs league, delta method | Goalies "DO NOT IMPROVE" with age; decline noticeable by 30; about -.004 per year at 35-38 and -.005 at 38-40 (linear fit) |
| [Tulsky 2012](https://theleafsnation.com/news/amp/goalie-aging-survivorship-bias) | Goalie save percentage | Flat average by age is survivorship bias; among those who lasted past 33 or 35, "goalies do decline with age" |
| Brander et al. (same paper) | Goalies | "little systematic performance variation over most of the age range from the early 20s to late 30s" |

Where they disagree: peak age ranges from 23 to 28 depending on the outcome (overall value peaks earlier than points) and on whether selection is modelled. All agree the curve is flat near the top, so the practical difference for ages 23 to 29 is small; the disagreement matters most for players under 22 and over 31. Goal scoring ages faster than playmaking. For goalies, the raw average hides decline that the within-player studies find.

## 6. In-season updating

The Bayesian framing: the preseason projection is the **prior** (what we believed before the season), season-to-date results are the evidence, and the updated projection is a weighted average with weight n / (n + k) on the evidence. The right k differs by stat, which is the main message of section 1.

Evidence on how fast early results become informative:

- For teams, the first 10 games' points explain 14.2% of the variation in final points; score-adjusted shot differential explains 28.9%; and "shooting and save percentages over ten games held almost no predictive power whatsoever" ([SI 2015](https://www.si.com/nhl/2015/11/07/hockey-analytics-predicting-season-10-games)).
- Team PDO after 1,000 shots (roughly 17 to 20 games) keeps 13% of its deviation ([Desjardins](https://www.arcticicehockey.com/pdo-regression-to-the-mean-or-why-you-should-ignore-shooting-1550520512/)). JFresh notes the same logic at midseason: a December 31 PDO still regresses, just "more softly" because fewer games remain ([JFresh](https://jfresh.substack.com/p/percentage-luck-in-hockey-explained)).
- For skaters, shot-based involvement (own shots plus shot assists) predicts rest-of-season primary points best after about 11 games, while primary assists themselves needed about 41 games to beat their own 8-game peak ([Stimson](https://hockey-graphs.com/2016/01/27/redefining-shot-quality-one-pass-at-a-time/)).

**Hot hand.** A review of the hockey literature reports that the one study of shooters (Vesper, 2011-12) found an adverse effect after a goal, and that only 1 of 31 teams showed a momentum benefit in 2018-19 (Steeger et al.) ([Chachad et al. 2024](https://pmc.ncbi.nlm.nih.gov/articles/PMC10800907)). For goalies, across 48,431 playoff shots, "we find no evidence for the hot-hand theory, and some evidence that good recent save performance negatively impacts the next-shot save probability" ([Ding et al.](https://arxiv.org/abs/2102.09689)). Paine reached the same conclusion from history: "We can't predict who will fluctuate, just that somebody likely will" ([Paine](https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie)).

**Role change versus results change.** No source gives a formula, so this is inference from the evidence above. Ice time, power-play share and linemates are inputs that projection authors set by assumption ([Yahoo](https://sports.yahoo.com/nhl-fantasy-hockey-how-to-make-sense-of-player-projections-165941054.html)); when they change, the old prior describes a different job. Opportunity is observed almost without noise (minutes are minutes), so it should update quickly, while per-minute conversion should keep its slow regression. I found no published table for "how much to move after 10, 20, 40 games" for individual fantasy production; the n / (n + k) weights with stat-specific k are the defensible answer, and our backtest is the right place to fit k.

## 7. Small samples and new situations

- **Team changes.** No rigorous study was found. A blog analysis reported that top-quartile players who moved lost about 0.16 points per game on average, mostly explained by changes in ice time and shots per game (unverified, search excerpt). Hohl notes that part of the persistence in a player's percentages comes from stable "team environments, linemates, roles, and goaltending" ([Hohl](https://thefivehohl.substack.com/p/what-does-regression-to-the-mean)), which implies on-ice percentages should be regressed harder after a move.
- **Call-ups and rookies.** Use an NHLe-translated prior with wide uncertainty; the published error is 0.33 points per game ([hockeystats.com](https://hockeystats.com/methodology/nhle)), which is large relative to the difference between a replaceable and a useful fantasy player.
- **Returning from injury.** Medical-journal studies conflict: some find lower ice time or scoring after return, several concussion studies find no change (unverified, search excerpts only; none read in full).

## 8. Evaluating a projection system honestly

- **Out-of-sample only.** Evolving-Hockey held out a whole season for its xG model ([Evolving-Hockey](https://evolving-hockey.com/?p=394)); the equivalent for projections is rolling origin, where each season is predicted with constants fitted only on earlier seasons.
- **Baselines.** Marcel is the customary simple baseline ([method: Tango](https://www.tangotiger.net/archives/stud0346.shtml)); garik16 notes "most forecasting systems don't improve upon Marcel by very much" ([Hockey Graphs](https://hockey-graphs.com/2014/02/03/forecasting-future-goalie-performance-with-four-year-hockey-marcels/)). Last season's rate and the positional mean are the other two.
- **Error metrics.** Mean absolute error and root mean squared error for level, rank correlation for ordering, and bias (mean signed error) to catch environment drift of the kind Clifford found. For probabilities, log loss and AUC as MoneyPuck reports.
- **Spread and calibration.** A well-regressed projection is correctly less spread out than outcomes. Evolving-Hockey rescaled its outputs to the observed spread and said plainly this is "not a standard technique" and does not change rankings ([Evolving-Hockey 2021](https://evolving-hockey.com/blog/2021-fantasy-hockey-projections/)). Rescaling makes projections look like real seasons but increases squared error; do not do it to the numbers used for decisions.
- **Overfitting.** Evolving-Hockey observed that a linear ridge model "would've likely performed quite well" against 16 gradient-boosted models. With roughly 10 to 15 usable seasons, a handful of fitted constants is all the data supports.

## How this applies to our model

Supported by the evidence:

- **Marcel-style 5/4/3 history with shrinkage and aging** is the standard, hard-to-beat design.
- **Very light weight on last 30/15/7 days** agrees with every hot-hand study found. The fitted recency weights in `docs/backtesting.md` (0.85 season / 0.15 last 30 / 0 / 0) are in line with the literature.
- **Shooting-luck correction toward xG** is supported at individual level (ixG beats shooting percentage).
- **On-ice shooting percentage correction on assists** is supported: on-ice shooting percentage repeats rarely, and secondary assists barely repeat.
- **Goalie start share as a separate input** matches Evolving-Hockey's volume-first approach.
- **A much larger shrinkage constant for goalies than skaters** (fitted k of 120 games versus 8 for forwards and 14 for defensemen) matches the direction of the evidence. As a rough check that is my own inference: 120 games at about 28 shots is about 3,400 shots, close to Paine's 3,000.

Questionable or worth testing:

- **One k per statistic, not per component.** (Corrected after checking the code, [09](09-engine-gap-analysis.md) D1: the engine shrinks each statistic's rate, not total fantasy points per game, and goals, assists and saves have their own regressions.) The evidence says components differ by an order of magnitude (shots and hits versus shooting percentage and secondary assists). A single k of 25 games for skaters still over-regresses volume statistics (shots, hits, blocks, penalty minutes) relative to conversion statistics; that remaining issue is 09's B8.
- **GSAx correction for goalies.** GSAx repeats no better than save percentage (r = 0.12), so a single season's GSAx should move the projection very little. (Corrected, 09 D2: the engine pools GSAx over three seasons and regresses it with 3,000 shots before rebuilding the save component, which is the treatment this file asks for. Goalies with fewer than 500 weighted shots fall back to k = 40 games.)
- **Regressing finishing to exactly xG.** A few elite shooters reliably beat xG; a career finishing ratio regressed with several hundred shots is better supported than 1.0.
- **In-season k of 25 games (skaters) and 40 (goalies).** For goalie rate skill the literature implies far more than 40 games; the fitted value is probably being driven by volume (who is starting), which is a role signal, not a skill signal.
- **Age factor for goalies.** (Corrected, 09 D5: the season projection has no goalie age factor at all; only the dynasty goalie curve is hand-set.) Published within-goalie studies show no improvement after the early twenties and decline from about 30, so the dynasty curve is the place to check.

Missing (two of the original five items turned out to exist; see 09 D3 and D4):

- A **role-change path** exists in the engine (09 D4); the open question is whether its size is right (09 B4). Ice time and power-play unit changes should update opportunity immediately, separately from rate regression.
- **Assist split**: primary and secondary assists regressed differently, especially for defensemen.
- **League-environment adjustment**: power-play opportunities and scoring level drift year to year and bias every player at once.
- **Team-change handling** for on-ice percentages and power-play role.
- **NHLe prior** for players with no NHL history when no provider projection exists. This exists in the engine (09 D3: `rookie.rs`, `nhle_factors.json`); it is listed here only as a design requirement the engine already meets.

## Implications for the engine

Well supported by sources:

1. Regress individual shooting percentage with about 375 shots of position-average shooting for forwards and 275 for defensemen, or equivalently toward individual xG. Test: backtest goals with and without; expect lower error with.
2. Treat secondary assists for defensemen as league-average rate times ice time; give forwards' secondary assists roughly half the trust of primary assists. Test: split assists in the season table and compare rest-of-season assist error.
3. Do not let last-7 or last-15-day results change a ranking by themselves. Check: recency weights for those windows stay at or near zero in every refit.
4. Goalie single-season save percentage or GSAx should receive weight of roughly n / (n + 3,000) shots. Check: the weight on 600 shots is 600 / (600 + 3,000) = 1/6, so a goalie with 600 shots of .930 (.034 above a .896 league mean) should project about .034 / 6 = .006 above his prior, that is within about .006 of the prior, not .003.
5. Always report a naive (last season) and Marcel baseline next to the engine in backtests, fitted on earlier seasons only. Already done by `fm backtest run`; keep it a release gate.
6. Track bias (projected minus actual) per season and per stat, not only MAE, to expose environment drift.

My own inference (not directly stated by a source):

7. Project components (shots, hits, blocks, goals-per-shot, primary assists, secondary assists, power-play points) with separate k values, then score them, and compare against the single-k FPG model under rolling origin. Keep it only if it wins out of sample.
8. Split every projection into opportunity (games, ice time, power-play share, goalie start share) times per-opportunity rate. Update opportunity on a short window (two to three weeks), rates on the slow n / (n + k) schedule.
9. After a team change, reset the on-ice shooting percentage and power-play components to the prior, and keep individual shot, hit and block rates.
10. For finishing, use career goals divided by career xG, regressed with a few hundred shots at 1.0, capped to a narrow band.
11. Bootstrap the fitted constants (already available via `--boot`) and refuse to promote a new fit whose improvement over the old constants is inside the bootstrap interval.
12. Add a goalie age factor of zero improvement at any age and a small decline from 30 (about .004 save percentage per year by the mid-thirties per garik16), then test it; the published samples are small.

## Open questions / where sources disagree

- **Does xG beat Corsi at team level?** Sprigings and Toumi say clearly yes after 20 games; the multi-site comparison says they are equal.
- **Is save percentage repeatable?** Paine's 0.296 and Hohl/Tulsky's "highly repeatable ... once sufficient sample size is met" versus JFresh's 0.12 on 18 goalies. Sample definitions differ; nobody has published a current, large-sample answer with GSAx.
- **Skater peak age:** 23 to 25 (value, delta method), about 26 (forwards, newer work), 27 to 29 (scoring, regression). Brander also finds no goalie age pattern, against Tulsky and garik16.
- **Are goals now more predictive than xG?** A summary of Luszczyszyn's Net Rating says so (unverified, search excerpt); not confirmed from a primary source.
- **Raw hits and blocks repeatability** and the size of arena scorekeeper effects on fantasy value: only possession-adjusted, two-season figures were found.
- **IPP, individual shot rate and goalie win rate** have no published stabilisation numbers that I could verify. These are worth measuring in our own season table.
- **How much a hot start should move an individual fantasy projection** at 10, 20 and 40 games is not published anywhere I found; it has to come from our in-season backtest.

## Sources

Read in full or in the relevant part:

- https://www.tangotiger.net/archives/stud0346.shtml
- https://www.arcticicehockey.com/pdo-regression-to-the-mean-or-why-you-should-ignore-shooting-1550520512/ (archived copy; the page carries a 2019 repost date)
- https://www.arcticicehockey.com/luck-vs-shot-quality-in-shooting-percentage-1550491415/ (archived copy, same caveat)
- https://thefivehohl.substack.com/p/what-does-regression-to-the-mean
- https://hockey-graphs.com/2015/10/01/expected-goals-are-a-better-predictor-of-future-scoring-than-corsi-goals/
- https://evolving-hockey.com/?p=394
- https://moneypuck.com/about.htm
- https://www.hockeyviz.com/txt/magnusSG
- https://arxiv.org/html/2511.07703v2
- https://pensionplanpuppets.com/sick-hands-shooting-talent-in-the-nhl
- https://theleafsnation.com/news/here-we-go-again-advanced-stats-and-analytics-primer-part-1
- https://jfresh.substack.com/p/percentage-luck-in-hockey-explained
- https://jfresh.substack.com/p/why-goaltending-is-basically-random?s=r
- https://neilpaine.substack.com/p/a-hot-goalie-isnt-a-better-goalie
- https://hockey-graphs.com/2014/02/03/forecasting-future-goalie-performance-with-four-year-hockey-marcels/
- https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/
- https://hockey-graphs.com/2014/03/21/how-well-do-goalies-age-a-look-at-a-goalie-aging-curve/
- https://theleafsnation.com/news/amp/goalie-aging-survivorship-bias
- https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/
- https://ar5iv.arxiv.org/html/2110.14017
- https://ideas.repec.org/a/bpj/jqsprt/v10y2014i2p19n6.html (abstract only)
- https://thehockeywriters.substack.com/p/how-does-the-production-of-nhl-forwards-change-with-age (free portion only)
- https://fooledbygrittiness.blogspot.com/2016/04/to-secondary-assist-or-not-to-secondary_15.html
- https://hockey-graphs.com/2016/01/27/redefining-shot-quality-one-pass-at-a-time/
- https://hockey-graphs.com/2016/01/14/special-teams-analytics-in-the-21st-century/
- https://www.habseyesontheprize.com/introducing-possession-independent-hitting-and-shot-blocking/
- https://hockey-graphs.com/2015/03/24/spread-of-nhl-shooting-performances-year-to-year-1952-53-through-2013-14/ (team-level only; not used for a claim)
- https://evolving-hockey.com/blog/2021-fantasy-hockey-projections/
- https://evolving-hockey.com/?p=692
- https://evolving-hockey.com/blog/wins-above-replacement-replacement-level-decisions-results-and-final-remarks-part-3/ (no predictive results in the text; not used for a claim)
- https://hockeystats.com/methodology/nhle
- https://hockey-graphs.com/2020/03/02/which-league-is-best/
- https://thibaudchatel.substack.com/p/goalies-nhle-and-league-equivalents
- https://dobberhockey.com/2025/05/23/ramblings-how-shots-power-play-opportunities-and-goal-distribution-affected-preseason-projections-may-23
- https://sports.yahoo.com/nhl-fantasy-hockey-how-to-make-sense-of-player-projections-165941054.html
- https://www.si.com/nhl/2015/11/07/hockey-analytics-predicting-season-10-games
- https://pmc.ncbi.nlm.nih.gov/articles/PMC10800907
- https://arxiv.org/abs/2102.09689 (abstract only)

Could not be opened (403, 404 or paywall), so nothing from them is stated as verified: the TSN on-ice shooting percentage and IPP articles, the HFBoards on-ice shooting thread, the "Hockey Marcels 2.0" Medium post, the core.ac.uk repeatability thesis, the James-Stein goalie paper, the spazznolo goalie posts, and The Athletic's GSVA / Net Rating explainers.
