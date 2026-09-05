# 0.18 FTS prefix review (#55)

Baseline: `ff3fa2d3278ef92fe040ad5d351dd6f83e51b685`, the pushed #53/#54 batch.
The owner asked to commit/push that batch and implement further justified 0.18 work.

## Decisions

- Reviewed the live #55 report against `FtsLiteral::tokenize`: prefixes bypassed the analyzer.
- Chose raw-token normalization with opt-in filters: Lowercase/LowerCase and AsciiFolding
  reuse index-time implementations and configured order. Full-word filters stay out.
- Kept posting scans, candidate restrictions, index layout and exact-term analysis unchanged.
- Moved quoted prefix boundary checking ahead of filters, including inside NEAR, so
  stopwords cannot hide an unsupported phrase. Empty prefix text emits no search term.
- The acceptance test reproduced an integer booster panic (`DI*^3`). Fixed the parser's
  `int`/`pos_int` mismatch and tested integer/decimal equivalence plus overflow rejection.
- The site still claimed quoted terms lack phrase semantics; corrected that already-shipped
  behavior and labeled the new prefix/boost fixes unreleased.

## Validation

- Full default engine suite: 752 passed, 0 failed, 8 existing ignored tests.
- Six prefix tests cover ASCII/Unicode, expanding lowercase, filter order (small-capital A),
  folding, stemming/stopword/length/compound boundaries, Simple/Raw/NGram, quoted/Boolean/
  NEAR forms, boosts, candidate eligibility, update/delete and SQLite reopen behavior.
- Final focused prefix/phrase/candidate suites: 29 passed; strict all-target Clippy passed.
- Fresh minimal Python extension: five smoke tests passed on macOS arm64, including
  prefix and integer-boost cases. Built from `cozo-lib-python/Cargo.toml` with macOS dynamic
  lookup linking; imported from a fresh temporary directory, not the installed package.
  The same script gates natively runnable wheel jobs before publication.
- Proximity documentation: all 26 runnable page blocks plus seed and inline-contract
  regressions passed (35 total). Placeholder syntax and four schema shapes are not runnable.
  Displayed deterministic outputs were compared with the harness; the graph-link example
  has the existing explicit random-layout caveat. Site production build passed.
- Rebased consumer batch against current core main: 426 tests passed, 0 failed, 2 ignored.
  PR #80's hosted CI uses published 0.17 for backward compatibility; local validation uses
  the new engine. Auto-merge is disabled in core, so merge requires completed checks.

## Release boundary

Include #53, #54, #55 and the reproduced integer-boost panic fix. Keep #56 behind a current
corruption reproduction, #11 export behind concrete demand, and #57 quantization behind
scale/quality evidence. No storage-format, bridge or dependency changes are needed.
Version remains 0.17.0 until the coordinated release bump. Package publication and the
site draft PR #3 are separate release gates; no deployment is claimed here.
