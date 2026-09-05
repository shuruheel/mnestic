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

Retain `parser.string_decoding_changed` for 0.18, with removal queued for 0.19. Emit at most once per warning drain (normally one script run), using the existing diagnostics sink. Qualifying double-quoted literals contain a backslash or edge trivia; qualifying raw literals contain edge trivia. Include only the literal's byte offset within the parsed input, never its contents. For FTS this is the FTS query text, not an offset translated into the enclosing script.

The message says **may decode differently**: valid FTS escapes and some hash-containing literals already decoded correctly. This is a conservative migration aid, not a complete compatibility detector. Standalone expression parsing follows the existing thread-local sink lifetime until a subsequent database flush.

Route `DbInstance::run_script` through the existing options dispatcher so both public entry paths use the same parser/error flush. Flush emitted diagnostics on AST-construction errors as well as execution errors. A grammar failure before literal decoding emits no warning. Test this explicitly across two databases on the same thread; warnings from a failed script must not leak into the second database.

## Acceptance

- Feature-ungated `tests/string_literals.rs`: both issue cases; each escape in each quoted form; BMP/non-BMP/surrogate boundaries; invalid escapes and byte offsets; empty strings; quote/fence substrings; mismatched and spaced fences; identifier adjacency; CRLF, hashes and block-comment markers; whitespace preservation; comments between tokens; hidden-quote compatibility case; parameters; `::describe`.
- Round-trip all 30,941 strings of length <= 4 over the 13-character grammar alphabet, plus 2,000 deterministic length-5–40 strings including Unicode and CR. Encode in all three forms, batch at most 1,000 rows, include row IDs so relational ordering/deduplication cannot hide a mismatch. No new dependency.
- FTS module tests pin escaped phrases, invalid-escape rejection and fenced-text interpretation.
- Warning tests pin deduplication, repeated runs, no warnings for unaffected forms, byte offsets after multibyte text, and failures during AST construction and execution.
- Run engine tests, strict default-feature Clippy, and MindGraph all-feature consumer tests. Audit in-tree CozoScript for changed literals and triage every hit; elapsed time alone is not a soak.
- Validate changed site examples through a freshly built `doc_check`; grammar edits must invalidate its build. Publish docs only with the corresponding release.

## Consumer work and release boundary

Replace the three MindGraph interpolated type lists with one parameter-building helper and test quoted/backslash-containing types. This is a separate consumer change; its current in-tree caller uses constants, so it does not create a downstream deployment gate for the engine.

The implementation and migration documentation shipped in 0.18.0 alongside the independently reviewed FTS prefix changes (#55). No bridge change was required. Keep the temporary migration warning removal scheduled for 0.19.0.

## Review decisions, 2026-09-05

Keep the draft's BMP-only contract, warning, exhaustive/seeded tests and consumer hygiene. Correct its character-offset wording, false-positive warning claim, and incorrect assumption that parse errors already flush diagnostics. Remove obsolete 0.17.0 release alternatives and calendar-soak language. Existing specs do not need to be commissioned again.
