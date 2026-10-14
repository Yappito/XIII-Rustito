# Rules for delegated implementation tasks

These rules apply to every delegated task (opencode or sub-agent) in this repo, in addition
to `AGENTS.md` and the task spec. A reviewer checks every result against them.

## Do what the spec asks, or say clearly that you did not

- The acceptance cases in the spec are the contract. If a case cannot be met, it is reported
  as **FAIL** with the evidence. Do not swap in an easier case (a different start point,
  input, map or threshold) and report PASS. You may add a clearly labelled extra case next to
  the requested one, but never instead of it.
- Do not tune parameters, tolerances or defaults until a test passes. A default that makes a
  test pass by hiding behaviour (skipping, clamping, catching and ignoring) is a bug.
- Every report has a **Deviations** section listing every place where you departed from the
  spec, with the reason. Write "none" only if that is true.

## Evidence discipline

- Label every claim in the report as one of: **measured** (with the command that produced
  it), **from source/upstream** (with the reference), or **hypothesis**. Do not present
  inferences as measurements.
- Before writing an explanation of a surprising number, check it against basic invariants:
  units and scale (a uniform scale factor changes no fit, ratio or clearance), coordinate
  axes and handedness, half vs full sizes, signs, and off-by-one. If an explanation depends
  on a quantity that cancels out, it is wrong.
- Surprising results are findings, not problems to make disappear. Report them with numbers.

## Tests that try to break the code

- For each non-trivial function, add at least one test aimed at a realistic failure mode
  (boundary, touching/overlapping start, degenerate input, float error, repeated small steps,
  empty input, maximum size), not only the happy path. Test behaviour, not implementation
  details.
- Opt-in game-data tests read the install at runtime and print `SKIPPED` without the env var.
  Never commit proprietary bytes, dumps or extracted data.

## Do not stop early

You run headless: nobody can answer questions or tell you to continue. Do not end your turn
after investigating or planning. Keep working until the spec is implemented, the acceptance
commands have run and the report is written. If you are genuinely blocked, write the report
explaining the block (what you tried, the exact error) and then end. Remove scratch/debug files
you created before finishing.

## Before you finish: self-review

1. Re-read the spec line by line and check off each requirement in the report (done / partial
   / not done, with where).
2. Run every acceptance command exactly as written and paste the result lines.
3. Read your own diff once, looking for: silent fallbacks, `unwrap`/`expect` on data from the
   game files, unused code, leftover debug output, files outside your ownership list.
4. Then write the summary. The first line of the summary states the honest overall status
   (for example "PASS except X", or "FAIL: Y"), never just "all green" when cases were changed or
   skipped.

## Boundaries

- Stay inside the files the spec lets you own. If you need to change something else, stop
  and explain in the report.
- Never modify, revert or delete files you do not own, by any means (edit tool, shell
  redirection, scripts). Other files in the tree may change while you work (the reviewer and
  other tasks edit them); that is expected. Leave them alone and mention them in the report if
  relevant. A permission denial is final: do not work around it.
- Never write to `XIII_Game/` or any game installation. No git commands that change history,
  branches or the index. No new dependencies unless the spec allows them.

## Tests must not depend on git-ignored files

Tests (unit or opt-in) must not read anything under `local/` (reports, scripts, notes): those files
exist only in one worktree and the test silently breaks everywhere else. Put fixtures you author
(play scripts, expected values) under the crate's `tests/data/` and commit them; game data is only
read through `XIII_GOG_DIR`/`XIII_STEAM_DIR`.

## Never touch the user's own game state

Tests and manual runs must not read or write the user's `%APPDATA%` (save slots, menu INI).
Tests create a fresh temporary directory. Manual `--play`/`--menu` runs pass `--save-dir` and
`--config-dir` pointing to a temporary directory: a checkpoint reached during a benchmark or a
route run otherwise writes a real save slot, and other tests or the user then see it.

## Keep memory bounded (shared 64 GB machine)

Several agents and one full test suite run at the same time. A single debug suite takes up to ~28 GB
and a release route test ~15 GB with two test threads. Run tests with `RUST_TEST_THREADS=1` unless
the spec says otherwise. Run only one heavy test or probe at a time in your worktree. Any diagnostic
binary or probe you build must bound its memory: load one map at a time, cap collections and
iteration counts, and stream output to a file instead of keeping it in memory. On 2026-10-13 an agent
probe grew to 64 GB and pushed the machine to 1.2 GB of free commit. A watchdog kills the largest
worktree process when free commit drops below 15 GB. If one of your test runs dies abruptly (exit
code 1 with no panic, or "process didn't exit successfully"), it was probably killed by the watchdog:
rerun it before treating it as a failure. Start `verify.sh` only as
`bash P:/AI/XIII/local/verify.sh --bg local/reports/<x>-verify.txt`. It detaches the suite and returns
at once. Never use `nohup` or `&`: an attached child can hang your session. Then poll that log with short
commands.

## Never stop processes you did not start

Other agents' tests, other worktrees' builds and the coordinator's landings run on the same machine.
Never use `Stop-Process`, `taskkill`, `pkill` or `killall`, and never kill processes by name (for
example every `xiii_app`). On 2026-10-13 an agent killed all `xiii_app` processes three times and
aborted two landings. If you must stop something you started in this session, record its PID when
you start it. Check that its working directory is your worktree, then kill only that PID.
