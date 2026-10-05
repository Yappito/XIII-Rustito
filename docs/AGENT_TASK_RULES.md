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
- Never write to `XIII_Game/` or any game installation. No git commands that change history,
  branches or the index. No new dependencies unless the spec allows them.
