# CozoScript string-literal correctness (#53)

Status: released in Mnestic 0.18.0 (commit `1d13aedc`). The owner authorized the implementation and reviewed contract on 2026-09-05. Replaces the 2026-08-28 draft (audit retained in git history).

## Outcome

Double-quoted literals decode the existing quoted escape grammar. Fenced raw strings preserve their contents, including comments and edge whitespace. Parameters remain the preferred way to pass values.

The 0.17.0 wheel reproduces escaped-quote and raw-hash parser failures, edge-whitespace trimming, and verbatim backslashes in ordinary double-quoted strings. The cause is the zero-underscore raw-string alternative plus implicit trivia skipping inside raw literals.

## Contract

1. Change only the raw-string grammar: make it compound-atomic (`${…}`), and require one or more underscores in its fence. Keep its inner pair, comment rules, FTS ordering and both quoted decoders' value semantics.
2. Double- and single-quoted forms recognize backslash, slash, their own quote, backspace, form feed, newline, carriage return, tab and `\uXXXX`. Each Unicode escape is a BMP scalar; lone surrogates and surrogate pairs remain `parser::invalid_utf8_code`. Literal non-BMP text works. Python JSON encoders must use `ensure_ascii=False` or bind parameters. Full JSON compatibility is not claimed.
3. Raw strings decode nothing. A fence has n >= 1 underscores on each side with no gaps. A quote followed by n or more underscores terminates the body; choose a longer fence when necessary.
4. Whitespace, CRLF and comment markers inside literals are data. Comments between tokens retain their existing behavior. Raw newlines/control characters in quoted strings remain accepted.
5. Unknown quoted escapes fail with `parser::pest` at the character after the backslash. A final backslash can escape the closing quote and produce an error later or at end of input. Invalid fences and identifier adjacency remain errors.
6. FTS valid quoted escapes remain supported. Invalid escapes no longer fall back to a zero-fence raw string. Fenced FTS text remains word/phrase/word syntax, not a new raw-phrase feature. Cypher is unchanged.

## Migration and diagnostics

This changes existing scripts, so it ships in a minor release with a migration note:

- Escaped double-quoted text changes value; use fenced raw strings for verbatim backslashes.
- Leading/trailing whitespace and comment-like text are preserved. Trim explicitly when intended.
- Gaps between a raw fence and its quote become errors.
- A comment inside a literal can no longer hide a quote. An old single literal can become multiple expressions; this case cannot be detected reliably by a literal-local warning.
- Re-executed `::describe` and stored-query bodies use the new parsing. Existing stored strings/descriptions are not rewritten.
- Parameters and single-quoted value semantics are unchanged.

The temporary `parser.string_decoding_changed` diagnostic shipped in 0.18 and is
removed in the 0.19 candidate. Decoding, error codes and stored values are unchanged.
The migration guidance above remains permanent. Absence of this retired diagnostic
is not proof that a legacy query has been migrated.

The diagnostic was conservative: valid FTS escapes and some hash-containing literals
already decoded correctly. The released-0.18 fixtures in
`cozo-lib-python/tests/test_string_migration_019.py` distinguish corrected parameter
builders from intentionally warning-producing literals and compare result/error
behavior with the candidate. The wheel workflow retains that isolated baseline.

Both public script entry paths still flush diagnostics on parsing/AST and execution
errors. Routing tests use a synthetic extension diagnostic and the permanent
`query.cartesian_step` diagnostic; failures must not leak warnings to another Db.

## Acceptance

- Feature-ungated `tests/string_literals.rs`: both issue cases; each escape in each quoted form; BMP/non-BMP/surrogate boundaries; invalid escapes and byte offsets; empty strings; quote/fence substrings; mismatched and spaced fences; identifier adjacency; CRLF, hashes and block-comment markers; whitespace preservation; comments between tokens; hidden-quote compatibility case; parameters; `::describe`.
- Round-trip all 30,941 strings of length <= 4 over the 13-character grammar alphabet, plus 2,000 deterministic length-5–40 strings including Unicode and CR. Encode in all three forms, batch at most 1,000 rows, include row IDs so relational ordering/deduplication cannot hide a mismatch. No new dependency.
- FTS module tests pin escaped phrases, invalid-escape rejection and fenced-text interpretation.
- Warning tests pin retirement of the migration signal; independent routing tests cover repeated runs, clear, grammar/AST/evaluation failures and database ownership.
- Run engine tests, strict default-feature Clippy, and MindGraph all-feature consumer tests. Audit in-tree CozoScript for changed literals and triage every hit; elapsed time alone is not a soak.
- Validate changed site examples through a freshly built `doc_check`; grammar edits must invalidate its build. Publish docs only with the corresponding release.

## Consumer work and release boundary

Replace the three MindGraph interpolated type lists with one parameter-building helper and test quoted/backslash-containing types. This is a separate consumer change; its current in-tree caller uses constants, so it does not create a downstream deployment gate for the engine.

The implementation and migration documentation shipped in 0.18.0 alongside the independently reviewed FTS prefix changes (#55). No bridge change was required. The 0.19 candidate removes only the temporary migration warning.

## Review decisions, 2026-09-05

Keep the draft's BMP-only contract, warning, exhaustive/seeded tests and consumer hygiene. Correct its character-offset wording, false-positive warning claim, and incorrect assumption that parse errors already flush diagnostics. Remove obsolete 0.17.0 release alternatives and calendar-soak language. Existing specs do not need to be commissioned again.
