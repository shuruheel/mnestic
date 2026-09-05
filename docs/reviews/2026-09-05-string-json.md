# #53/#54 implementation review

Baseline: Mnestic 0.17.0, commit `786c3a8f7bb2b9a646be08aa76422700a2b550b7`.
The owner authorized planning cleanup, spec review/improvement and implementation on September 5.
This is local implementation evidence, not merge, hosted CI, release or deployment evidence.

## Review changes

- Condensed the existing drafts into normative contracts; the original audits remain in git history.
- Kept BMP-only Unicode escapes, NaN-to-null, the bounded migration warning and the existing
  snapshot/JSON boundaries. Removed obsolete 0.17.0 placement alternatives.
- Corrected diagnostic offsets to bytes; an unknown escape points at the character after the
  backslash. The warning says "may decode differently" because its detector is conservative.
- Found that both Db and DbInstance could return before flushing AST-construction warnings.
  DbInstance now uses the existing options dispatcher; the shared path flushes errors into the
  originating Db. A two-database regression checks this boundary.
- Documented that infinity values already reduced to JSON null cannot be reconstructed without
  another source. Added a SQLite reopen fixture proving old JSON is preserved beside new forms.
- Replaced the proposed snapshot Debug-string heuristic with exported-column type checks and
  a populated export/import round trip containing quote/backslash/Unicode user text.

## Scope

Mnestic: raw-string grammar, migration diagnostic, shared JSON conversion, JSON-column coercion,
regression tests, migration changelog and reviewed specs. No dependencies or bridge changes.

MindGraph: three wiki/entity type-list queries use a parameter helper; snapshot format remains
unchanged and is guarded by schema/round-trip tests.

Site: Types and Functions references, explicitly labeled unreleased, plus retained runnable
examples. Overlay: current engine allocation and a doc_check wrapper that asks Cargo to check
all build inputs instead of maintaining an incomplete source-extension freshness filter.

## Literal census

The published 0.17.0 wheel reproduced the two reported parser failures, whitespace trimming,
verbatim backslash-n and nested UUID arrays before implementation. Current source tests pin the
new behavior.

Searches covered engine/Python source and tests, MindGraph source/tests, and site MDX. Broad
Rust string matches were host-language query separators, diagnostics, fixtures or serialization
output. The three interpolated type-list builders were replaced. A separate raw-Rust-string
scan of CozoScript bodies found no remaining escaped double-quoted or fenced literal inputs
outside the new string suite. On the site, the affected input is the Types escape example;
other escaped matches are syntax prose or displayed output. New regression fixtures are
intentional. This census covers the local source tree, not external users' scripts or stored
query catalogs.

## Validation

| Check | Result |
|---|---|
| `cargo test -p mnestic` | 746 passed, 0 failed, 8 existing ignored tests; includes doc tests and the 98,823 literal round trips |
| `cargo clippy -p mnestic --all-targets -- -D warnings` | Passed after correcting a test seed's digit grouping (numeric seed unchanged) |
| `cargo test --all-features` in mindgraph-rs | 424 passed, 0 failed, 2 existing ignored tests, including doc tests |
| `cargo clippy -p mindgraph --all-features --all-targets -- -D warnings` | Passed |
| Fresh minimal-feature Python extension | 4 compatibility tests passed on macOS arm64 / Python 3.13.7 |
| Types / Functions examples | 32 + 32 blocks passed; the final Cargo-run wrapper was exercised |
| `pnpm build` in mnestic-site | Passed |
| Wheel workflow | YAML parsed and test-script wiring checked; hosted execution is pending |
| Whitespace / shell | Scoped `git diff --check` and `bash -n doc_check` passed |

Rust checks used `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0` and
`CARGO_INCREMENTAL=0` to fit disk. Consumer, docs and Python builds shared the engine target
directory. Only regenerable artifacts were cleaned; unrelated source changes were preserved.
Rust Analyzer could not become ready and started an unnecessary native build; that session's
analyzer/build was stopped and navigation continued through CodeGraph plus compiler validation.

Python build command (macOS extension-module linking):

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo rustc \
  --manifest-path cozo-lib-python/Cargo.toml --no-default-features --features minimal \
  --lib -- -C link-arg=-undefined -C link-arg=dynamic_lookup
```

The resulting `libmnestic.dylib` was copied as `mnestic.so` into a temporary import directory;
`cozo-lib-python/tests/test_string_json_contract.py` ran against that extension, not the installed
0.10.1 package. This is not a published wheel or the complete release matrix. The same smoke test
is now wired into every natively runnable wheel job before publication. Existing native RocksDB
and PyO3 dependency warnings were observed; no binding or bridge code was changed to address them.

The validation above was completed before the implementation commits. Delivery and hosted CI
are tracked separately from this local validation snapshot.

Release gates remain: review/merge, exact-commit hosted CI, 0.18.0 version and registry-readme
updates, package publishing/verification, coordinated site publication, and removal of the
migration warning in 0.19.0. #55 is separately queued; #11 import is already released and export
remains demand-gated. No tracker mutations are part of this change.
