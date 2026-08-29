# Spec — CozoScript string-literal correctness: JSON escapes in double-quoted strings, `#` inside raw strings (mnestic #53)

_Created 2026-08-28. Source: [shuruheel/mnestic#53](https://github.com/shuruheel/mnestic/issues/53) (upstream cozodb/cozo#223 and cozodb/cozo#234), `docs/strategy/MNESTIC-ROADMAP.md` backlog row 4. Every `file:line` below is relative to the `mnestic/` checkout unless prefixed `mindgraph-rs/` or `.claude/` (the workspace-root `docs/strategy/*.md` and `versions.toml` cited by the roadmap gate, §5 and S25 live in the parent of `mnestic/`, not inside it), and was read against the working tree on 2026-08-28 (HEAD = 0.16.0 era, `cozo-core/Cargo.toml:3`). Behavioural claims marked **(run)** were executed against the installed `mnestic` 0.10.1 Python wheel. The string and comment rules (`cozoscript.pest:72-75`, `:193-219`) and the three decoders (`parse/expr.rs:451-513`) are byte-identical from `fork-base` to HEAD — the pest diff (45+/13−) is confined to other rules; its only hunk touching that region (`@@ -192,3 +220,6 @@`, `git diff -U0 fork-base HEAD -- cozo-core/src/cozoscript.pest`) begins after `string = …` with the `boolean`/`null` word-boundary fix — so the wheel's parser is the working tree's parser for this surface._

**Status: DRAFT — awaiting owner review**

- **Provenance:** Pass 1 code-facts grounding (3 lenses) · Pass 2 lean draft · Pass 3 bounded panel — 26 findings (10 major, 16 minor; 3 were duplicates of another finding), **26 folded, 0 rejected** · Pass 4 prior-art grounding — 7 verdicts (5 CONFIRMS with refinements folded, 1 CONTRADICTS folded as D7, 2 GAPs folded) + 1 essential omission folded as D7 · Pass 5 simplification — 9 requirement-neutral cuts applied (listed in §6.2), 4 owner-ruling cuts listed below, not applied. Triage record: §6.1.
- **Blockers open: 0.** Two decisions were taken here that the owner may reverse at review without re-running the panel: D2's BMP-only `\u` contract (surrogate pairs stay rejected; the alternative is ≈20 lines in two decoders, R11) and D7's one-release detection warning (the prior-art fold; the alternative is the pass-2 note-only shape, R6).
- **Owner-ruling cuts (not applied):** (1) drop D7 and ship note-only, as the pass-2 draft did — prior art says no, lean discipline could say yes; (2) delete the dead `raw_string` alternative from `fts_phrase` (`:297`) — a third grammar token, cosmetic; (3) drop the seeded 2,000-string half of D5.4 and keep only the exhaustive ≤4 enumeration plus the D5.2 table; (4) drop B3 (mindgraph-rs `$types` hygiene) from this spec into an ordinary mindgraph-rs commit with no batch row — it is not a prerequisite for the tag.
- Status is set by the owner: APPROVED after this review; SHIPPED after the B4 tag. This spec never self-approves.

> **Roadmap gate, verbatim** (`docs/strategy/MNESTIC-ROADMAP.md:32`):
> `| 4 | String-literal correctness (#53) | Ready for design | 3 | 6 | Migration note and soak for changed decoding behavior |`
>
> Both halves of the gate are deliverables here: the migration note is D3, the soak is D6 (with D7 as its external-consumer signal). Cx 3 / Ci 6 bounds this to a grammar-modifier change plus a bounded warning, tests, docs and one consumer hygiene conversion — not a lexer or decoder rewrite. No storage-format or public-API change (issue #53, "Scope").

---

## 1. Verified current state

The issue reports two failures. Reading the grammar and running the engine shows they are two symptoms of **one** rule and **one** modifier, plus a third, unreported behaviour (silent whitespace trimming) that the same rule causes and that the fix necessarily changes.

| # | Claim | Where | Consequence |
|---|---|---|---|
| S1 | `string` is an ordered choice that tries `raw_string` first: `string = _{(raw_string \| s_quoted_string \| quoted_string)}` | `cozo-core/src/cozoscript.pest:219` | PEG commits to the first alternative that *succeeds*. `quoted_string` is reached only when `raw_string` fails outright. |
| S2 | `raw_string`'s fence is `PUSH("_"*)` — zero or more underscores | `cozoscript.pest:208` | A bare `"…"` matches `raw_string` with an empty fence. **Every ordinary double-quoted literal in expression position is parsed as a zero-underscore raw string.** |
| S3 | `raw_string` and `raw_string_inner` are plain `{ }` rules — neither atomic `@{` nor compound-atomic `${` — while `quoted_string` and `s_quoted_string` are `${` | `cozoscript.pest:207`, `:212` vs `:193`, `:200` | pest inserts the implicit `WHITESPACE`/`COMMENT` skip between every sequence element and repetition of a non-atomic rule, so `WHITESPACE` (`:72`) and `LINE_COMMENT`/`COMMENT` (`:74-75`) are live *inside* the raw-string body. |
| S4 | **(run)** `?[x] <- [["blah\"\""]]` → `parser::pest` at `17..17`; `?[x] <- [["a\"b#"]]` → `parser::pest` at `14..14` | issue #53 case 1; S1+S2 | The zero-fence raw string closes at the first inner `"` (`blah\`), the enclosing list fails on the leftover, and PEG does **not** re-enter `string` to try `quoted_string`. Root cause is S1+S2, not the escape set. |
| S5 | **(run)** `?[x] <- [[___"#298594"___]]` → `parser::pest` at `13..13`; but **(run)** `_"a # x⏎ b"_` → `a # x⏎ b` (no error) while `_"a⏎ b # x"_` → `parser::pest` | issue #53 case 2; S3; `LINE_COMMENT = _{ "#" ~ (!"\n" ~ ANY)* }` (`:74`) | After the opening `"`, the implicit skip runs `LINE_COMMENT`, which eats to end-of-line. **When the `#` is on the closing quote's line** the closing `"` is gone and the parse fails; when it is on an earlier line of a multi-line literal the comment ends at `⏎`, the literal still closes, and the value is verbatim (the span covers the skipped text). Root cause is S3. |
| S6 | **(run)** `?[x] <- [["#298594"]]` → `#298594`; `?[x] <- [["a#b"]]` → `a#b`; **`?[n] := n = length("a#\nb")` → `4`**; but **`length("x#⏎y\tz")` → `7`** | S1+S3; `cozoscript.pest:193-199` | The brief's assumption that the zero-fence case fails identically is **wrong**: a `#` on the closing quote's line makes `raw_string` *fail* (comment eats the closing quote), the choice falls through, and `quoted_string` — with full JSON escape decoding — succeeds. So today **a `#` on the closing line silently switches escape decoding ON** for that literal (`a#\nb` is 4 chars: the `\n` became a newline). A `#` on an earlier line does not: `x#⏎y\tz` stays 7 verbatim characters. |
| S7 | **(run)** `?[x] <- [[" a "]]` → `a`; `[["a\n"]]` (real newline) → `a`; `[["a /* x */ "]]` → `a`; `[[__" a "__]]` → `a`; but `[["a  b"]]` → `a  b`, `[["a /* c */ b"]]` → `a /* c */ b`, `[["/* x */a"]]` → `a`, and `[[" a#b "]]` → ` a#b ` (fall-through to the atomic `quoted_string`, S6) | S3 | **Unreported defect:** the implicit skip before `raw_string_inner` and before the closing `"` strips leading and trailing whitespace (and leading/trailing block comments) from every double-quoted and fenced raw string; interior characters survive because a pest span covers everything between a rule's start and end. |
| S8 | **(run)** `?[x] <- [[__ "abc" __]]` → `abc` (whitespace between fence and quote accepted) | S3 | A currently-valid spelling that any atomicity fix rejects. Must appear in the migration note. |
| S9 | **(run)** `"a\nb"` → 4 chars (`length` = 4 vs 3 for `'a\nb'`); `"a\qb"` → `a\qb`; `"a\\"` → 3 chars; `"\t"` → 2 chars; **`?[x, y] <- [["a\", "b"]]` → `a\`, `b`** (a `"…"` ending in a backslash is valid today) | S1+S2; `parse/expr.rs:509-513` | Double-quoted literals decode **nothing** today. `parse_raw_string` returns the inner span verbatim. |
| S10 | `char` already implements the JSON escape set: `!("\"" \| "\\") ~ ANY \| "\\" ~ ("\"" \| "\\" \| "/" \| "b" \| "f" \| "n" \| "r" \| "t") \| "\\" ~ ("u" ~ ASCII_HEX_DIGIT{4})`; `s_char` mirrors it with `\'` | `cozoscript.pest:195-199`, `:202-206` | The double-quoted escape **contract is already written**. Any fix that edits `char` changes nothing; the job is to make `quoted_string` reachable. Both rules admit raw newlines and control characters (laxer than RFC 8259). The parser runs over a Rust `&str` (`parse/mod.rs:43-44`) and `ANY` (`:196`, `:216`) consumes one Unicode scalar value, so the unit of every literal is the **character**, never a byte; no literal form can express invalid UTF-8. |
| S11 | `parse_quoted_string` (`:451-478`) decodes the nine escapes, raises `parser::invalid_utf8_code` (`:443`) for an unencodable `\uXXXX` and `parser::invalid_escape_seq` (`:448`) for any other `\`-prefixed token; `parse_s_quoted_string` (`:480-507`) is its `'` twin; `parse_raw_string` (`:509-513`) is verbatim. **Each `\uXXXX` is decoded independently through `char::from_u32` (`:465-470`, `:494-499`) — no surrogate-pair joining.** | `cozo-core/src/parse/expr.rs` | **(run)** `"a\qb#"` → `parser::pest` at `12..12`; `"a\uD800#"` → `parser::invalid_utf8_code`: the grammar rejects unknown escapes first, so `invalid_escape_seq` is a defensive arm; `invalid_utf8_code` **is** reachable. **(run)** `'\uD83D\uDE00'` → `parser::invalid_utf8_code` (55357): a well-formed JSON surrogate pair is rejected in the single-quoted form today, and `"\uD83D\uDE00"` yields the 12 raw characters. Python's `json.dumps` emits exactly that pair for every non-BMP character by default (`ensure_ascii=True`). |
| S12 | `parse_string` (`:431-439`) is the single dispatch on `Rule::quoted_string \| s_quoted_string \| raw_string \| ident`; consumers: `build_expr` (`parse/expr.rs:262`), FTS `build_phrase` (`parse/fts.rs:94-95`), `::describe` (`parse/sys.rs:236`) | `cozo-core/src/parse/` | One grammar change propagates to every string consumer through one function; no call-site edits. |
| S13 | `string` is referenced from two grammar positions — `literal` (`:243`, hence every `term`, `:153`) and `describe_relation_op` (`:53`); `fts_phrase` (`:297`) references the three underlying rules directly | `cozo-core/src/cozoscript.pest` | The compatibility enumeration (D3) is closed over these three positions. Validity clauses (`:104`) take `expr`, so they inherit through `literal` — no separate form. |
| S14 | `fts_phrase = {(fts_phrase_group \| quoted_string \| s_quoted_string \| raw_string) ~ …}` (`:297`) tries `fts_phrase_group` first, and `fts_phrase_simple = @{… (XID_CONTINUE+)}` (`:293`) matches `_` (U+005F is XID_Continue). **The `raw_string` arm is unreachable, before and after D1.** **(run)** with bodies `a b c`, `#tag x`, `x z`, `C:\Users`, `___ y` indexed: FTS `___"#tag"___` → `[[2]]` (parsed as word `___` AND phrase `#tag` AND word `___`, no error); `_"tag"_` → `[[2]]`; `___` → `[]`; `__"zzz" __` → `[]`, no error. Separately, a `"…"` phrase with a non-JSON escape **falls through to the empty-fence raw string**: FTS `"C:\Users"` → `[[4]]` (raw text matched), `"a\qb"` → `[]` with no error. | `cozoscript.pest:293-297`; `parse/fts.rs:94-95` | Raw strings are not available as FTS phrases today and D1 does not make them so (fenced text is tokenised as words). The one FTS behaviour D1 changes is the empty-fence fall-through: it disappears with `_+`, so a `"…"` phrase containing a bad escape becomes a hard `parser::pest` (C12). mindgraph-rs cannot reach either: `sanitize_fts_query` keeps only alphanumerics (`mindgraph-rs/src/storage/cozo.rs:6593-6611`). |
| S15 | Cypher has an independent grammar and decoder: `string = @{…}` is atomic (`cypher/cypher.pest:82`), `COMMENT` uses `//` and `/* */` only (`:18`), and `unquote` (`cypher/parse.rs:511-542`) passes unknown escapes through (`:534`), accepts `\0` (`:523`) and substitutes U+FFFD for a bad `\uXXXX` (`:530`) | `cozo-core/src/cypher/` | Unaffected by both issue cases; a third, laxer escape contract exists in the fork. Precedent that atomic string rules are the house pattern. |
| S16 | `#[grammar = "cozoscript.pest"]` on `CozoScriptParser` (`parse/mod.rs:43-44`); `pest = "2.8"` with a load-bearing floor comment (`cozo-core/Cargo.toml:155-159`); lock resolves pest 2.8.7 (`Cargo.lock:3216-3217`) | one grammar file, one parser | `@`/`$` trivia suppression, `PUSH`/`PEEK`/`POP` and ordered-choice commitment are pest 2.8 semantics — no dependency change. Atomicity cascade is documented pest behaviour: `ParserState::atomic` sets `self.atomicity` for the whole closure ("we take atomicity of the top rule and not of the leaf", `pest-2.8.7/src/parser_state.rs:1439-1455`), every nested `state.rule(...)` inherits it, and `PUSH`/`PEEK` do not touch that mechanism. |
| S17 | No test anywhere covers raw strings or double-quoted escapes: `parse/mod.rs:531` tests error diagnostics only; `parse/fts.rs:149` tests FTS structure; `grep '___"'` over `cozo-core/src`, `cozo-core/tests`, `cozo-lib-python`, `cozo-bin`, `mindgraph-rs/src` = 0 hits; `[dev-dependencies]` (`Cargo.toml:217`) has no proptest/quickcheck/arbitrary | mnestic | Regression net is greenfield; nothing in the suite pins the wrong behaviour, so every migration risk is in *user* scripts. `serde_json` (`Cargo.toml:139`) is a normal dependency and gives a JSON encoder for the round-trip test without a new dev-dep. |
| S18 | The public docs state the current behaviour as the contract: "Double-quoted strings … take every character between the quotes exactly as typed, backslashes included"; the example pins `length("a\nb")` = 4 (`[3,4]`); a warning callout says escapes are processed "**only in single-quoted strings**" and that this diverges from the upstream manual | `mnestic-site/src/app/docs/types/page.mdx:287-311` | Documented behaviour changes. `page.mdx:295` is the only doc listing whose value changes; the raw-string example at `:314` (`___"a "quoted" word"___`) is unchanged. |
| S19 | mindgraph-rs interpolates double-quoted literals into CozoScript at exactly three byte-identical sites, all `.map(\|t\| format!("\"{}\"", t)).join(", ")` feeding `is_in(node_type, [{type_list}])`: `src/storage/cozo.rs:10520-10524` and `:10579-10583` (hard-coded ASCII `article_types`, `:10508-10519`) and `:10651-10655` inside `pub fn top_connected_entities(&self, types: &[&str], …)` (`:10641`, script at `:10669`), whose `types` are **caller-supplied** via `MindGraph::top_connected_entities` (`src/graph.rs:1365`) / `AsyncMindGraph` (`src/async_graph.rs:866`, `Vec<String>`); the only in-tree caller passes a hard-coded local `Vec<String>` (`mindgraph-server/src/wiki.rs:1637-1651`, call at `:1660`) duplicating `ARTICLE_ELIGIBLE_TYPES` (`:136`) | `mindgraph-rs/` | One site can receive arbitrary strings from a library caller; no untrusted input reaches it in-tree today. Today a `\` in a type name is passed through verbatim; after the fix it would decode or error. Convert all three to `$params` (D4). |
| S20 | Every other mindgraph-rs interpolation is single-quoted and either a compile-time constant (`TRAVERSAL_EXCLUDED_EDGE_TYPES`, `cozo.rs:78` used at `:1176`) or a whitelisted `[A-Za-z0-9_]` identifier (`:4022-4030` guard before `:4042`; same guard at `:6987-6994`, `:7011`, `:7054`, `:7077`); values go through `str_val(...)` parameters (`:14169`; 459 sites, e.g. `:9353` feeding `:9358`, `:14133` feeding `:14140`); user FTS text is reduced to alphanumerics before reaching the FTS grammar (`:6593-6611`; `:14907` test) | `mindgraph-rs/src/storage/cozo.rs` | Single-quoted decoding is untouched; no untrusted value reaches a double-quoted literal. `mindgraph-server/src` has no double-quoted interpolation into script text (grep hits are log/prompt strings only); `mindgraph-cloud/src` constructs no CozoScript. |
| S21 | Corpus census (this pass, by hand): no `.rs`, `.py` under `mnestic/`, `mnestic-site/`, `mindgraph-rs/` contains a fenced raw string, and the only `.md`/`.mdx` hit is the docs example `mnestic-site … types/page.mdx:314` (`___"a "quoted" word"___`, S18 — a proper fence, unchanged by D1); the only double-quoted CozoScript literal with a backslash is `mnestic-site … types/page.mdx:295` (input; value changes). Three further pattern hits are **output-only listings, not scripts**: `types/page.mdx:318` (result JSON), `functions/page.mdx:514` (result JSON), `tips/page.mdx:88` (an `::explain` output table). Rust `format!("\"{}\"", t)` literals in `cozo.rs` (S19) match the same regex and are not CozoScript. | this pass | Beyond `page.mdx:295`, no in-tree script changes meaning. The user-facing corpus (PyPI/crates consumers) is unknowable from here — hence D6 and D7. |
| S22 | **(run)** `?[x] := x = "a /* " ++ " */ b"` → **one** string `a /* " ++ " */ b`; `?[x] := x = "a # " ++ "⏎ b"` → one string `a # " ++ "⏎ b` | S3; `cozoscript.pest:73-74` | Inside today's zero-fence raw string a block or line comment **hides a `"`**: the comment is skipped, the literal runs on. After D1 the hidden quote terminates the literal and the remainder re-pairs — a today-valid script whose meaning changes silently (C3b). Undetectable by any per-literal check (the re-paired script carries no marker). |
| S23 | The fork already has a typed warning channel: `crate::runtime::diagnostics::emit(code, message, hint)` (`runtime/diagnostics.rs:50`) pushes into a thread-local sink; `Db::flush_warnings` (`runtime/db.rs:723`) drains it into a 256-entry ring (`WARNING_RING_CAP`, `:384`) at the end of **every** script run, success or error (`:714`), after `parse_script` (`:633`) has run on the same thread; `::warnings` / `Db::recent_warnings` (`:742`) read it. Five emitters exist (`query/reorder.rs:648`, `fixed_rule/algos/pagerank.rs:62`, `runtime/stored_queries.rs:446`, `runtime/graph_projection.rs:1158`, `runtime/db.rs:4070`); codes are dotted and "new ones may appear in any release" (`CHANGELOG-FORK.md:421-441`) | mnestic | A parser-side warning reaches every binding with no FFI, no knob and no grammar change — the existing primitive D7 reuses. |
| S24 | The docs harness `doc_check` takes **one** `.cozo` file (`.claude/skills/mnestic-docs/scripts/doc_check:4`, `:23-26`) and rebuilds only when a `*.rs` under `cozo-core/src` is newer than the binary (`:30-33`); there is **no** checked-in `.cozo` page corpus — `find mnestic-site mnestic/docs .claude/skills/mnestic-docs -name '*.cozo'` returns only `references/seed.cozo`; the skill's own method says to develop each page's examples in a `.cozo` file kept with the work (`SKILL.md:44-53`) | `.claude/skills/mnestic-docs/` | A `.pest`-only change would not trigger the rebuild, and "every `.cozo` page file" iterates over nothing. D6.2 names real inputs and forces the rebuild. |
| S25 | CI: `build.yml` runs on push/PR only (`:3-7`); the string suite would run in the `cargo test -p mnestic --verbose` step (`:25`, feature-ungated per the warning at `:26-28`). The two scheduled workflows are `planner-guard.yml` (nightly cron `:22`, also `pull_request:` at `:24`; LSQB execution tier `--test lsqb --ignored`, `:67`) and `dependency-freshness.yml` — neither runs the string suite. Release cadence: "mnestic ships independently and often (liveness-first)" (`versions.toml:11`); 0.17.0 is the pending merged-import release (`MNESTIC-ROADMAP.md:29`) | mnestic | "Days on main with nightly green" adds no signal beyond the merge-time run; a downstream deploy gate would be an upward dependency. D6.3 is one sentence. |

---

## 2. Decisions

### D1 — Grammar change: two edits on one rule, nothing else

**Seam:** `cozo-core/src/cozoscript.pest:207-208` (`raw_string`). No change to `char`/`s_char` (`:195-206`), `quoted_string`/`s_quoted_string` (`:193`, `:200`), `string` (`:219`), `fts_phrase` (`:297`), `LINE_COMMENT`/`BLOCK_COMMENT`/`COMMENT` (`:73-75`), or `raw_string_inner` (`:212-217`).

```pest
raw_string = ${                    # was: raw_string = {
    PUSH("_"+) ~ "\""              # was: PUSH("_"*)
    ~ raw_string_inner
    ~ "\"" ~ POP
}
```

1. **`PUSH("_"+)`** — a raw string needs at least one underscore. A bare `"` can no longer match `raw_string`, so `string`'s ordered choice reaches `quoted_string` for every `"…"` literal (fixes #223 / S4). In expression and `::describe` position the three forms become unambiguous on their first character: `'` → single-quoted, `"` → double-quoted, `_` → raw. The fence is unbounded (Rust caps `r#…` at 255 hashes; with `PUSH`/`POP` an unbounded `_+` is harmless and no cap is added). This is pest's own stack-idiom example (Rust raw strings via `PUSH("#"*)`/`POP`); Rust affords zero hashes only because the `r` prefix disambiguates — without a prefix, `_+` *is* the prefix.
2. **`${`** — `raw_string` becomes compound-atomic. pest's implicit `WHITESPACE`/`COMMENT` skip is suppressed inside it, and atomicity cascades into the plain `raw_string_inner` by documented pest semantics (S16: `ParserState::atomic` holds the top rule's atomicity for every nested rule; only an explicit `!{}` rule stops the cascade) — the same construct as `quoted_string = ${…}` over `quoted_string_inner = { char* }` (`:193-194`), which is what makes `"a#b"` come back intact via the fall-through in S6. `raw_string_inner` keeps its `{ }` body so `parse_raw_string`'s `into_inner().next()` (`expr.rs:511`) still finds the inner pair (an `@{` on the outer rule would erase it). Fixes #234 (S5), the whitespace trim (S7) and the fence-gap acceptance (S8).

Expected values are the "After D1" column of §4; B1's suite is the verification. Why not only (2): atomicity alone leaves `"blah\"\""` broken (S4 is independent of trivia). Why not only (1): a fenced `___"#…"___` still fails (S5). Why not reorder `string` instead of (1): §6 R1.

### D2 — Decoding contract per form (normative source = existing code)

**Seam:** `cozo-core/src/parse/expr.rs:431-513` (`parse_string` + three helpers) — **no decoder-semantics change**; this decision pins what the grammar routes to them. (D7 adds a warning emission to two of them; it never alters a returned value.)

| Form | Grammar | Decoder | Recognised escapes | Cannot contain |
|---|---|---|---|---|
| Double-quoted `"…"` | `quoted_string` `:193`, `char` `:195-199` | `parse_quoted_string` `:451` | `\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t` `\uXXXX` — the RFC 8259 §7 escape grammar, **BMP only**: each `\uXXXX` is one code point, a surrogate pair is `parser::invalid_utf8_code`, no `\U` | an unescaped `"`; a `\` not followed by one of the nine |
| Single-quoted `'…'` | `s_quoted_string` `:200`, `s_char` `:202-206` | `parse_s_quoted_string` `:480` | same nine with `\'` in place of `\"`; same BMP-only rule (unchanged since `fork-base`) | an unescaped `'`; a stray `\` |
| Raw `_…_"…"_…_` (n ≥ 1 underscores each side, equal) | `raw_string` `:207` (per D1) | `parse_raw_string` `:509` | **none** — the characters between the delimiters are the value | a `"` followed by **n or more** underscores (`PEEK` matches a prefix, `:214`; **(run)** `__"a"___` → `parser::pest`) |

Contract statements:

- **Laxer than JSON, deliberately:** both quoted forms accept raw newlines, `␍`, and control characters inside the literal (`char`'s first alternative admits any character but `"` and `\`). Single-quoted strings have always behaved this way; double-quoted strings match them. Raw strings admit `␍` too (Rust forbids CR in raw strings; we do not — §4 has a CRLF row). No change.
- **Unit is the character.** Scripts are UTF-8 `&str` (S10); no form can express invalid UTF-8 or a bare byte.
- **Unknown escapes are a parse error at the backslash** (C4), code `parser::pest`, offset = the `\` (S11). A `"…"` literal that *ends* in `\` is a different failure: `\"` consumes the closing quote and the literal runs to the next `"` or end of input (C5, remote offset). `parser::invalid_escape_seq` stays as a defensive arm; the spec does not promise it. `parser::invalid_utf8_code` is the one decoder error a user can reach (`"\uD800"`, and any surrogate pair).
- **BMP only, stated where users read it:** RFC 8259 §7 says `"\uD834\uDD1E"` *is* U+1D11E; this engine rejects it, exactly as `'…'` always has. Python callers must use `json.dumps(s, ensure_ascii=False)` or `$params`; every other client should pass non-BMP text raw (`serde_json::to_string` already does). The note, `page.mdx` and §4 carry `"😀"`-shaped rows. Joining pairs is R11 (owner-reversible).
- **Raw strings are the escape hatch:** a raw string can represent every string except one containing its own closing delimiter; choosing n greater than the longest run of underscores following any `"` in the content always works (I1). This is Rust's `r#"…"#` rule and PostgreSQL's `$tag$…$tag$` rule with `_` for the tag.
- **`#`, `/*`, `*/`, `//` are ordinary characters inside all three forms.** Comment recognition happens only between tokens. Enforced by atomicity (D1.2), not by editing `COMMENT` — the same mechanism PostgreSQL (`'this -- is not a comment'`) and SPARQL §19.4 use: the string token is lexed as one unit.
- **Identifier adjacency:** `x_"a"_` and `_x_"a"_` are parse errors today and after D1 (**(run)**; `term` tries `literal` before `var`, `:153`; after `_+` fails at the required `"` the choice falls to `var` and the leftover `"a"_` fails) — PostgreSQL's dollar-quote-after-identifier rule, pinned as a §4 row rather than left as a note.
- **FTS phrase literals** (`fts_phrase`, `:297`): `quoted_string` and `s_quoted_string` are live and decode as in expression position; the `raw_string` alternative is **dead code before and after D1** (S14) — raw strings are not available as FTS phrases, a fenced query is tokenised as bare words. Left as is (R10; owner-ruling cut 2). The one FTS change is C12.
- **Cypher is out of scope, explicitly.** `cypher/parse.rs:511 unquote` keeps its lenient contract (`\q` → `q`, `\0` accepted, bad `\u` → U+FFFD). Recording the divergence here is the deliverable; aligning it is a separate row because the Cypher surface is alpha and feature-gated (`docs/specs/cypher-read.md:3`) and has no reported bug.

### D3 — Compatibility: what changes meaning, and the migration note

**Seam:** the three positions of S13 — `literal` (`:243`) in every expression, `::describe` (`:53`, `parse/sys.rs:236`), `fts_phrase` (`:297`) — plus `mnestic-site/src/app/docs/types/page.mdx:287-311` and `CHANGELOG-FORK.md` (Unreleased).

The #234 half is a widening **only for single-line fenced strings**: a `#` on the closing line of a fenced raw string always ended in a hard parse error (S5). A multi-line fenced string with `#` on an earlier line parses today (S5) and only changes under the trim rule (C10). The #223 half changes the value of currently-valid scripts. Exhaustive enumeration:

| Class | Script today | Value today | After D1 | Kind |
|---|---|---|---|---|
| C1 | `"…"` containing one of the nine escapes, e.g. `"a\nb"`, `"C:\\x"`, `"\u0041"`, `"a\b"`; includes a multi-line `"…"` with `#` on an earlier line and an escape later (`"x#⏎y\tz"`, S6) | backslash kept (`a\nb` = 4 chars; `x#⏎y\tz` = 7) | decoded (`a⏎b` = 3 chars; `C:\x`; `A`; `a` + U+0008; 6 chars) | **silent value change** — the roadmap's "changed decoding behavior"; D7 warns |
| C2 | `"…"` with leading/trailing whitespace or newlines, e.g. `" a "`, a multi-line literal starting with `⏎` | trimmed to `a` (S7) | preserved: ` a ` | **silent value change** (unreported today); D7 warns |
| C3 | `"…"` with a leading/trailing block comment, e.g. `"/* x */a"` | `a` (S7) | `/* x */a` | silent value change; no in-tree occurrence; D7 warns |
| C3b | `"…"` whose body contains a block or line comment that itself contains a `"`, e.g. `"a /* " ++ " */ b"` (S22) | one string `a /* " ++ " */ b` | two strings: `a /* ` ++ ` */ b` = `a /*  */ b` | silent value change, **remainder re-pairs; not detectable** (§7.1) |
| C4 | `"…"` with `\` before any other character, e.g. `"C:\Users"`, `"a\qb"` | raw characters | `parser::pest` at the `\` | **loud** — parse error at a precise offset |
| C5 | `"…"` ending in a backslash, e.g. `"a\"`, `"C:\Users\"` | `a\` (valid, S9) | `\"` escapes the closing quote → the literal runs to the next `"` → `parser::pest` **at end of input or a later token**, not at the backslash (**(run)** single-quoted analogue: `?[x] <- [['a\']]` → `16..16` = EOI) | **loud, remote offset** — the least diagnosable class; named in the note |
| C6 | `"…\"…"` (issue case 1) | error at the leftover | works | widening (#223) |
| C7 | `"…#…"` with the `#` on the closing quote's line | already decoded via fall-through (S6) | decoded | **unchanged** — but `" a#b "` now keeps its spaces (C2) |
| C8 | fenced `_"…#…"_` with the `#` on the closing line (issue case 2) | error | works | widening (#234) |
| C9 | fenced with a gap: `__ "abc" __` (S8) | `abc` | `parser::pest` at the space | loud |
| C10 | fenced with leading/trailing whitespace or comment inside: `__" a "__`; includes multi-line fenced strings with `#` on an earlier line (S5) | `a` | ` a ` | silent value change (same mechanism as C2); D7 warns |
| C11 | `::describe rel "…"` with any of C1–C5 | stored as today's value | stored as the new value **only when re-executed**; existing stored descriptions are untouched (no storage change) | as C1–C5, persisted |
| C12 | FTS phrase `"…"` containing a `\` not followed by one of the nine escapes (S14) | silently accepted as raw text via the empty-fence fall-through | `parser::pest` | **loud** on the direct-mnestic FTS surface; unreachable from mindgraph-rs (S14) |
| C13 | FTS phrase `"…"` with valid escapes, or `_"…"_` in an FTS query | already `quoted_string`; fenced text tokenised as words (S14) | identical | none |
| C14 | `$params`, single-quoted `'…'`, ordinary line/block comments between tokens, `""`, `__""__` | — | identical (**(run)**: `?[x] <- [["a"]] # c` and `""` both work today and are covered by D5) | none |

**Migration note (ships in `CHANGELOG-FORK.md` Unreleased and the release readme, following the FTS-phrase entry's shape at `CHANGELOG-FORK.md:353-365` — named error where one exists, workaround in the text, external citation):**

> **Double-quoted strings now decode JSON escapes; raw strings are comment-proof.** Fixes upstream cozo#223 / cozo#234 (mnestic #53).
>
> - `"…"` literals follow the RFC 8259 §7 escape grammar — `\" \\ \/ \b \f \n \r \t \uXXXX` — exactly as `'…'` already did. `\uXXXX` is one BMP code point; a surrogate pair (`"\uD83D\uDE00"`) is rejected with `parser::invalid_utf8_code`, as it always was in `'…'`. Pass non-BMP text raw (Python: `json.dumps(s, ensure_ascii=False)`) or as a `$param`.
> - `_"…"_` raw strings (one or more underscores, equal on both sides) take every character verbatim, including `#`, `/*` and newlines. A bare `"…"` is no longer a raw string. The only thing a raw string cannot contain is a `"` followed by as many (or more) underscores as its fence — use a longer fence.
>
> **Behaviour changes for existing scripts:**
>
> 1. A `"…"` literal containing one of the nine escapes **changes value**: `"a\nb"` is now 3 characters, not 4; `"C:\\x"` is now `C:\x`. Silent — check your scripts for `"` … `\` … `"`; for one release the engine reports each such script once as warning `parser.string_decoding_changed` (`::warnings`).
> 2. A `"…"` literal containing a backslash before any other character is now a **parse error at the backslash** (`parser::pest`). Rewrite as `'…'` with `\\`, or as a raw string: `_"C:\Users"_`. The same applies to a `"…"` phrase inside a full-text query.
> 3. A `"…"` literal **ending in a backslash** (`"C:\Users\"`) is now unterminated: the `\"` escapes the closing quote and the literal continues to the next `"`; the parse error points at the end of input or a later token, not at the backslash. Write `\\"` or use a raw string.
> 4. Leading and trailing whitespace (including newlines and block comments) inside `"…"` and raw strings is now **preserved**; it was silently trimmed (also reported by the warning above).
> 5. Whitespace between a raw string's underscores and its quote (`__ "abc" __`) is no longer accepted.
> 6. A comment inside a `"…"` literal no longer hides a `"` it contains — the quote now closes the literal. This case cannot be warned about; grep for `"` … `/*` … `"` … `*/`.
> 7. `::describe` descriptions re-executed after upgrade store the new value; stored descriptions are untouched until then.
>
> Unaffected: `$params`, single-quoted strings, comments between tokens. Parameters remain the recommended path for untrusted input. Precedent: JSON (RFC 8259) for the quoted forms; Rust `r#"…"#` and PostgreSQL dollar-quoting for the fenced raw form; PostgreSQL's `escape_string_warning` for the one-release warning.

The `page.mdx:287-311` paragraph, example (`[3,4]` → `[3,3]`) and warning callout are rewritten in the same commit; the callout is replaced by a note that the engine now matches the upstream manual, with the BMP-only sentence.

### D4 — Blast radius into mindgraph-rs: three interpolation sites become one parameter helper

**Seam:** `mindgraph-rs/src/storage/cozo.rs:10520-10524`, `:10579-10583`, `:10651-10655` (S19).

- All three `is_in(node_type, [{type_list}])` sites (script text at `:10669` for `top_connected_entities`) become `is_in(node_type, $types)` with `params.insert("types", DataValue::List(types.iter().map(|t| str_val(t)).collect()))` through one small helper; `top_connected_entities` already carries `params` for `min_edges`/`limit` (`:10661-10662`). No double-quoted interpolation of a type list remains in mindgraph-rs, so no audit note has to be maintained.
- Nothing else changes: S20 shows every remaining literal is single-quoted (untouched by D1) and either constant or `[A-Za-z0-9_]`-guarded; all values go through `$params`. `mindgraph-server` and `mindgraph-cloud` build no double-quoted literals. FTS user text is reduced to alphanumerics upstream (`cozo.rs:6593-6611`), so neither S14 nor C12 can surface through `/retrieve`.
- No mindgraph release is required by this change; the conversion ships in the next ordinary mindgraph-rs commit and is *not* a prerequisite for the mnestic tag (the in-tree caller passes constants).

### D5 — Tests: one new integration suite, no new dependency, `mem` backend throughout

**Seam:** new file `cozo-core/tests/string_literals.rs` (house convention: one suite per spec, cf. `fts_phrase.rs`, `pareto_skyline.rs`; `fork_regressions.rs` is the naming precedent for upstream-bug regressions). Picked up by the existing `cargo test -p mnestic --verbose` step (`.github/workflows/build.yml:25`) with no workflow edit — it must stay feature-ungated, per that workflow's own warning (`:26-28`). Parsing precedes backend dispatch and no stored-relation join is involved, so the `spec_doc_validation.rs:9-12` sqlite rule does not apply; **(run)** `::describe` + `::relations` work on `mem` (description column read back as `l1\nl2` today), so the whole suite runs on `mem`.

1. **Issue regressions** (issue #53 scope line 1), asserting the post-fix contract; the §4 table is the record of the pre-fix values.
   - #223: `?[x] <- [["blah\"\""]]` → `blah""`.
   - #234: `?[x] <- [[___"#298594"___]]` → `#298594`; `#` at start, middle, end of body; `#` immediately before the closing delimiter; `/* … */` and `*/` inside a raw body (the runtime pin for the atomicity cascade, S16); a newline and a `␍␊` inside a raw body; a multi-line raw body with `#` on its first line (`_"a # x⏎ b"_` → `a # x⏎ b`, unchanged) and on its last line (`_"a⏎ b # x"_` → `a⏎ b # x`, was error).
2. **Per-form decoding table** — one `run_script` per row of D2: each of the nine escapes individually in `"…"` and in `'…'` (the D5.4 encoder never emits `\/`, `\b`, `\f` or `\u`, so this table is their only coverage — by design); `"\u0041"` → `A`, `"\u00e9"` → `é`; `"\uD800"` and `"\uD83D\uDE00"` → `parser::invalid_utf8_code`; `"😀"` raw → `😀`; `"a\qb"` and `"C:\Users"` → `parser::pest` with the error offset asserted at the `\`; `"a\"` → `parser::pest` with the offset asserted at **EOI** (the remote position of C5, pinned); `""`, `''`, `_""_`, `___""___`; delimiter-like substrings (`"a'b"`, `'a"b'`, `__"a"_"__` → `a"_`, `__"a"_b"__` → `a"_b`); `__"a"___` → error (the n-or-more pin); leading/trailing whitespace preserved in all three forms (C2/C10); `"x#⏎y\tz"` → 6 chars; `__ "abc" __` → error (C9); fence mismatch `_"a"` and `"a"_` → error; identifier adjacency `x_"a"_` and `_x_"a"_` → error.
3. **Comments still comments:** `?[x] <- [["a"]] # tail`, a `# line` between rules, `/* block */` between tokens, and `"a" /* c */ , "b"` inside a list — all parse and yield the expected rows. Plus the C7 pin `" a#b "` → ` a#b ` and the C3b pin `x = "a /* " ++ " */ b"` → `a /*  */ b`.
4. **Round-trip property test, bounded, batched, dependency-free:** for every string over the alphabet `{a, ", ', \, #, _, /, *, ␠, ⏎, n, u, 0}` of length ≤ 4 (1 + 13 + 13² + 13³ + 13⁴ = 30,941 strings, empty string included), plus 2,000 strings of length 5–40 from a seeded xorshift generator over that alphabet extended with `é`, `😀` and `␍` (seed fixed in the test), encode each with (a) `serde_json::to_string` for `"…"`, (b) a local single-quote encoder mirroring `s_char`, (c) a raw fence of `1 + max run of _ after any "` underscores — and assert the original value comes back for all three encodings. Shape: batched, `?[x] <- [[lit1],[lit2],…]` in chunks of ≤ 1,000 literals per script per encoding, compared against the expected list (≈ 99 scripts total; measured 3.2 ms per 1,000-literal script on the 0.10.1 release wheel, ≈ 0.3 s; debug-profile pest is slower but stays well inside the default job). Hard cap is the enumeration bound, stated in the test's doc comment.
5. **FTS surface:** `parse_fts_query` (`parse/fts.rs:19`, crate-private → test lives as a `#[cfg(test)]` addition to `parse/fts.rs:149`'s module) accepts `"a \"b\" c"` as a phrase with kernel `a "b" c`, `is_phrase = true`; `"C:\Users"` → error after D1 (C12; today a phrase with the raw text); `___"#tag"___` → three `fts_expr`s (word, phrase `#tag`, word) both before and after, pinning S14.
6. **`::describe` round-trip:** `::describe t "l1\nl2"` stores a two-line description (today the six characters `l1\nl2`, backslash included — read back through `::relations`, description column), and `::describe t _"x # y"_` stores `x # y` (today: `parser::pest`).
7. **D7 warning:** `?[x] <- [["a\nb"], ["c\td"]]` yields exactly one `parser.string_decoding_changed` in `::warnings` (first literal's offset in the message); `" a "` and `__" a "__` yield one each; `"abc"`, `'a\nb'`, `_"a\nb"_` and `$params` yield none; a failing script that contains an escaped literal still leaves the warning in the ring (flush on error, `db.rs:714`).

### D6 — Soak plan (the second half of the gate)

The change turns errors into successes *and* silently alters values (C1–C3, C10), which is why a plain test pass is not the gate. What the gate's "soak" consists of — stated so it is closed by evidence that exists:

1. **In-tree census = S21.** Done by hand in this pass; the B1 PR description pastes the grep (`grep -rnE '"[^"]*\\[^"]*"' … --include='*.mdx' --include='*.rs' --include='*.py'`, plus the `_+"` fence pattern and the multi-line shapes `"` … (`/*`|`#`) … `"` … (`*/`|⏎) … `"`) and its hit list with each hit classified — expected: `types/page.mdx:295` (C1), the fenced raw-string example `types/page.mdx:314` (valid fence, unchanged), the three output-only listings of S21 and the Rust `format!` literals of S19 (not scripts). No engine test file walks consumer paths (that would point the dependency upward). **Stop condition:** any hit not already listed in S21 is triaged before merge.
2. **Consumer suites against the patched engine.** `cargo test -p mnestic` (as `build.yml`), then `cd mindgraph-rs && cargo test --all-features` against the sibling path dep, then `.claude/skills/mnestic-docs/scripts/doc_check types.cozo` where `types.cozo` is the new example file B2 adds (the listings of `types/page.mdx:287-320`, per the skill's step 4/5) — after forcing the harness rebuild (`cargo build --manifest-path .claude/skills/mnestic-docs/scripts/harness/Cargo.toml`; B2 also widens the guard at `doc_check:30` to `\( -name '*.rs' -o -name '*.pest' \)` so a grammar-only change can never be validated against a stale binary again). Transcripts pasted in the B2 PR. **Stop condition:** any listing whose output differs from the rewritten page.
3. **Release vehicle.** Rides **0.18.0** — 0.17.0 is the pending merged-import release (`MNESTIC-ROADMAP.md:29`) and this change does not hold it; never a patch. No calendar hold, no nightly clause, no downstream-deploy gate (S25).
4. **External-consumer signal = D7** for one release, plus the migration note. Not part of the soak: no telemetry, no compatibility flag, no dual-decoding mode.

### D7 — One-release detection warning for silently changed literals

**Seam:** `cozo-core/src/parse/expr.rs:451` (`parse_quoted_string`) and `:509` (`parse_raw_string`), emitting through the existing `crate::runtime::diagnostics::emit` (`runtime/diagnostics.rs:50`, S23); one new 6-line `emit_once(code, …)` helper beside it that skips if the thread-local sink already holds that code. ≈ 25 lines total; no grammar, no `Db` knob, no storage change, no change to any returned value.

- **Code:** `parser.string_decoding_changed` (dotted, per the house convention at `CHANGELOG-FORK.md:421-441`, whose table gains a row).
- **Fires when** a `"…"` literal contains at least one escape (any `char` pair whose text starts with `\` — its value differs from the 0.16 value, C1), or when the inner span of a `"…"` or fenced raw literal starts or ends with whitespace, or starts with `/*` or ends with `*/` (would have been trimmed, C2/C3/C10). Never for `'…'` (unchanged), `$params` (never lexed), or a `"…"` with no escape and no edge trivia. C3b is not detectable (S22) and is not claimed.
- **Bound:** at most **one** warning per script run — the first qualifying literal, with its character offset in the message; the ring cap (256, `db.rs:384`) is the outer bound as for every other code. Message: `double-quoted/raw literal at <offset> is decoded differently since 0.18.0 (JSON escapes applied; edge whitespace kept)`; hint: `if the old verbatim text was intended, write it as a raw string _"…"_; if the new value is intended, ignore — this warning is removed in 0.19.0`.
- **Reach:** every binding, via `::warnings` / `Db::recent_warnings` — including `parse_fts_query` (same thread, same drain). Literals parsed outside a script run (the Python `eval_expressions` path) sit in the thread-local sink until that thread's next flush, per the existing contract (`db.rs:718-722`).
- **Lifetime:** ships in the same minor as D1 (0.18.0); **deleted in 0.19.0** — a one-line `CHANGELOG-FORK.md` removal entry, added to the Unreleased section at tag time so it cannot be forgotten. Codes are "match the ones you know, ignore the rest" (`CHANGELOG-FORK.md:440-441`), so removal is not a breaking change.
- **Why a warning and not a flag (R6):** the decoded value is the new value regardless; nothing is threaded through `Db`; there is no second decoder and no second migration. Precedent: PostgreSQL's `escape_string_warning` (8.1, on by default), the detection signal it judged necessary before flipping `standard_conforming_strings` in 9.1 — the only production precedent for exactly this change.

---

## 3. Invariants

- I1 **Representability.** For every `&str` value `s`, at least one literal form encodes `s`: JSON-escaped `"…"` always does for BMP text and raw non-BMP text; `'…'` likewise; a raw string does iff `s` does not contain `"` followed by `n` or more underscores for the chosen `n` (choose `n` larger than any such run).
- I2 **Form is decided by the first character — in expression and `::describe` position.** `'` → single-quoted, `"` → double-quoted, `_+"` → raw. No fall-through between forms exists after D1.1. In FTS position the bare-word group is tried first, so a leading `_` is a word and raw strings do not exist there (S14).
- I3 **Comment recognition never crosses a string boundary.** Inside any of the three forms, `#`, `//`, `/*`, `*/` are data. Between tokens, `LINE_COMMENT`/`BLOCK_COMMENT` behave exactly as at `cozoscript.pest:73-75` today.
- I4 **Whitespace inside a literal is data.** No trimming, in any form.
- I5 **Raw strings are character-verbatim.** `parse_raw_string` returns the inner span unchanged (`expr.rs:509-513`); the only unrepresentable sequence is a `"` followed by n or more underscores.
- I6 **Escape sets are closed and shared.** Double- and single-quoted forms accept the same nine escapes (differing only in which quote is escapable); anything else after `\` is a parse error. The set is the RFC 8259 §7 escape grammar, BMP-only, and is not extended (no `\0`, no `\x`, no `\U`, no surrogate joining).
- I7 **One decoder entry point.** All string consumers route through `parse_string` (`expr.rs:431`); no consumer re-implements decoding for CozoScript (Cypher is a separate language with its own documented contract, D2).
- I8 **Parameters are unaffected.** `$x` binding bypasses the lexer entirely; nothing in this spec changes a bound value.
- I9 **No storage or API change.** Stored `::describe` text and every relation's bytes are untouched by the upgrade; only re-executed scripts see the new decoding.
- I10 **D7 never changes a value.** The warning is emitted beside the decoded result; removing it in 0.19.0 changes no parse.

---

## 4. Acceptance tests

Each row is a `cozo-core/tests/string_literals.rs` test unless marked otherwise; `✓` marks rows pinned by an engine run in this pass (pre-fix behaviour, the record of the flip), `→` the post-fix expectation the test asserts.

| Script | Today ✓ | After D1 → |
|---|---|---|
| `?[x] <- [["blah\"\""]]` | `parser::pest` 17..17 | `blah""` |
| `?[x] <- [[___"#298594"___]]` | `parser::pest` 13..13 | `#298594` |
| `?[n] := n = length("a\nb")` | 4 | 3 |
| `?[n] := n = length("x#⏎y\tz")` (multi-line, `#` on line 1) | 7 | 6 |
| `?[x] <- [["\u0041"]]` | `\u0041` | `A` |
| `?[x] <- [["a\qb"]]` | `a\qb` | `parser::pest`, offset at `\` |
| `?[x] <- [["a\"]]` (C5) | `a\` | `parser::pest` at EOI (16..16) |
| `?[x] <- [["a\uD800"]]` | `a\uD800` | `parser::invalid_utf8_code` |
| `?[x] <- [["\uD83D\uDE00"]]` (JSON surrogate pair) | 12 raw chars | `parser::invalid_utf8_code` (BMP-only, D2) |
| `?[x] <- [["😀"]]`, `[['😀']]`, `[[_"😀"_]]` | `😀` (all three) | `😀` |
| `?[x] <- [[" a "]]` | `a` | ` a ` |
| `?[x] <- [[__" a "__]]` | `a` | ` a ` |
| `?[x] <- [[__ "abc" __]]` | `abc` | `parser::pest` |
| `?[x] <- [["a#b"]]`, `[[" a#b "]]` | `a#b`, ` a#b ` | same (C7) |
| `?[x] := x = "a /* " ++ " */ b"` (C3b) | `a /* " ++ " */ b` (one string) | `a /*  */ b` (two strings) |
| `?[x] <- [[_"a /* c */ b # d"_]]` | `parser::pest` | `a /* c */ b # d` |
| `?[x] <- [[_"a # x⏎ b"_]]` / `[[_"a⏎ b # x"_]]` | `a # x⏎ b` / `parser::pest` | `a # x⏎ b` / `a⏎ b # x` |
| `?[x] <- [[_"a␍␊b"_]]`, `[["a␍␊b"]]` (CRLF inside) | 4 chars | 4 chars |
| `?[x] <- [["a"]] # tail` / block comment between tokens | `a` | `a` |
| `?[x] <- [[""]]`, `[['']]`, `[[__""__]]` | `` | `` |
| `?[x] <- [[__"a"_"__]]`, `[[__"a"_b"__]]` | `a"_`, `a"_b` | `a"_`, `a"_b` |
| `?[x] <- [[__"a"___]]` (n or more) | `parser::pest` | `parser::pest` |
| `?[x] <- [[_"a"]]`, `[["a"_]]` | `parser::pest` | `parser::pest` |
| `?[x] := x = x_"a"_`, `[[_x_"a"_]]` (identifier adjacency) | `parser::pest` | `parser::pest` |
| `?[x] <- [['a\nb']]` (single-quoted, control) | 3 chars | 3 chars |
| round-trip enumeration (D5.4) | n/a | all 30,941 + 2,000 strings round-trip through all three encodings, batched |
| `parse_fts_query("\"a \\\"b\\\" c\"")` (`parse/fts.rs` module test) | phrase `a "b" c` | phrase `a "b" c`, `is_phrase = true` |
| `parse_fts_query("\"C:\\Users\"")` | phrase with raw text `C:\Users` | `parser::pest` (C12) |
| `parse_fts_query("___\"#tag\"___")` | word `___`, phrase `#tag`, word `___` | identical (S14 pin) |
| `::describe t "l1\nl2"` then `::relations` | `l1\nl2` (6 chars, backslash kept) | `l1⏎l2` (5 chars) |
| `::describe t _"x # y"_` then `::relations` | `parser::pest` | `x # y` |
| `?[x] <- [["a\nb"], ["c\td"]]` then `::warnings` (D7) | no such code | exactly one `parser.string_decoding_changed` |
| `?[x] <- [["abc"], ['a\nb'], [_"a\nb"_]]` then `::warnings` (D7) | no such code | none |
| mindgraph-rs `top_connected_entities(&["Per\"son"], 1, 5)` (D4; mindgraph-rs test) | parse error | `Ok(vec![])` — the type is bound, not lexed |

---

## 5. Layering ruling

Per `docs/strategy/LAYERING.md` (checklist 1–5, `:31-35`): this is lexer/grammar **mechanism** in the engine — a stranger running a fraud graph hits the same two bugs, it carries no cognitive vocabulary, it is stable, and it is pulled by a real consumer report (#53, filed from MindGraph usage) *and* fits the "better CozoDB" wedge (two long-open upstream issues). D7 rides the engine's own diagnostics channel, a mechanism that already exists. **Verdict: mnestic — this spec IS a mnestic spec.** MindGraph's only obligation is the D4 hygiene conversion, which is independent of the engine change. Nothing moves across the seam; no primitive is added; no engine behaviour is shaped by a MindGraph concept; no engine file references a consumer path.

---

## 6. Rejected alternatives

- **R1 — Reorder `string` to `quoted_string | s_quoted_string | raw_string` and keep `PUSH("_"*)`.** Fixes #223 for well-formed literals, but a `"…"` with an invalid escape (`"C:\Users"`) would make `quoted_string` fail and *fall through* to the zero-fence raw string, silently yielding raw text — exactly what the FTS surface does today (S14, C12). Two spellings for one lexical form with data-dependent meaning is what I2 forbids. Rejected: requiring `_+` makes the form decidable from the first character.
- **R2 — Make `raw_string` `@{` atomic.** Erases the inner pair; `parse_raw_string` (`expr.rs:511`) `unwrap()`s it. Would need a decoder edit to slice the span manually. `${` is the zero-decoder-change modifier and matches `quoted_string`'s existing form.
- **R3 — Edit `char` to "add escapes".** `char` already carries the full JSON set (S10); the rule is unreachable, not incomplete. An implementer who patches here ships nothing. Recorded so the change is not re-attempted at the wrong seam.
- **R4 — Scope the `#` fix to `LINE_COMMENT` (e.g. only treat `#` as a comment at line start or after whitespace).** Changes comment semantics language-wide for a bug that is purely a string-rule atomicity omission; would break `x = 1 # c`. No production language checked (PostgreSQL, SPARQL) does this — every one makes the string token atomic instead. Rejected.
- **R5 — Keep zero-fence raw strings by adding a fourth form (e.g. `r"…"`).** New syntax for a behaviour nobody documented as wanted (the mnestic-site callout at `page.mdx:306` describes it as an accident relative to the upstream manual). Fenced `_"…"_` is a one-character migration. Rejected as speculative surface.
- **R6 — Compatibility flag / dual decoding for one release.** Two decoders for the same literal, a runtime knob to thread through `Db`, and a second migration later. Rejected as machinery for a customer that does not exist. What prior art (PostgreSQL 8.1→9.1) actually required was a *detection signal*, not a second decoder — that is D7, which costs no knob and no grammar. If the owner cuts D7 (owner-ruling cut 1), R6 stands unchanged and §7.1's user-corpus risk reverts to unmitigated.
- **R7 — Align the Cypher `unquote` decoder in this spec.** Different language, different grammar, alpha and feature-gated, no bug report. Recording the divergence (D2) is enough; a merge would widen a Cx-3 change into two surfaces.
- **R8 — Add `proptest` for the round-trip test.** New dev-dependency and, per `build.yml:26-28`, a new explicit CI step if feature-gated. A bounded exhaustive enumeration plus a seeded generator gives the same coverage with no dependency and a stated cap (D5.4).
- **R9 — Extend the escape set (`\0`, `\x41`, `\U0001F600`).** Not JSON; not what the upstream manual promises; adds a third contract. The 8-digit `\U` form is a divergence from the whole neighbourhood (SPARQL 19.7, Neo4j Cypher, PostgreSQL E-strings all offer it), recorded as such; still out of scope — the raw form and `$params` carry any code point.
- **R10 — Reorder `fts_phrase` to `(quoted_string | s_quoted_string | raw_string | fts_phrase_group)` so raw strings become live FTS phrases.** A second grammar edit and a new compatibility class (today `___"x"___` in FTS = AND(word `___`, phrase `x`, word `___`); after = phrase `x`) for a capability nobody asked for; mindgraph-rs strips the syntax before it reaches the engine. Rejected; the dead arm is left in place (deleting it is owner-ruling cut 2).
- **R11 — Join well-formed `\uXXXX` surrogate pairs in the two quoted decoders** (≈ 10 lines each at `expr.rs:465` and `:494`; lone surrogate stays `invalid_utf8_code`). Would make `"…"` fully RFC 8259 and rescue Python's default `json.dumps` output. Rejected for this spec because the failure is **loud** (`parser::invalid_utf8_code`, not a silent value), the contract is the one `'…'` has had since `fork-base`, the workaround is one keyword argument or `$params`, and it would relax D2's "no decoder-semantics change" and the Cx 3 bound. Owner-reversible at review without re-running the panel; if reversed, D5.2 gains the pair as a success row and D2's table drops "BMP only".

### 6.1 Panel triage

All 26 panel findings folded (0 rejected); the three duplicates (FTS-unreachable ×2, n-or-more ×2, atomicity-cascade ×2 counted once each in their twin's fold) folded together. Where a finding offered a choice, the choice taken:

- **FTS `raw_string` unreachable (major, ×2):** option (b) — keep D1's single seam; C13 rewritten to "identical", raw-string FTS tests dropped, S14/§4 corrected to "group + phrase + group, no error", R10 records the reorder. I2 qualified to expression/`::describe` position.
- **FTS empty-fence fall-through (major):** C12 rewritten as loud; note item 2 extended; §4 and D5.5 gain `"C:\Users"` → error.
- **Multi-line `#` (major):** S5/S6/C7/C8 qualified with "on the closing quote's line"; multi-line shapes filed under C1/C10; "pure widening" replaced; C3b added with S22 evidence; census shapes extended; §4 gains both scripts.
- **C5 missing from the note (major):** note item 3 added; C5 kind is "loud, remote offset"; D2 scopes "at the backslash" to C4; D5.2 pins the EOI offset.
- **Surrogate pairs (major):** option (b) — BMP-only stated in the D2 table, the note and `page.mdx`; `"😀"` and the pair are §4 rows; D5.4's generator gains `😀`/`é`; R11 records option (a).
- **doc_check corpus / rebuild guard (major, ×2):** D6.2 rewritten — forced rebuild, `*.pest` guard widening, `types.cozo` as the named input; D5.7 deleted (the Rust suite pins only its own table).
- **Census test (major):** deleted; D6.1 = S21 + PR-description paste; layering §5 states no engine file references a consumer path.
- **D6.3 (major):** reduced to one sentence naming 0.18.0.
- **Minor citation fixes:** S19/D4 (`wiki.rs:1637-1650`, `:1660`; `node_type` at `cozo.rs:10669`), S21 (`tips/page.mdx:88`), D5 (`build.yml:25`, `:26-28`), S15 (`:523`), header provenance and S13 (pest diff scope; `:297` references the three rules), n-or-more wording (D2, note, I5), `__"a"_"__` replaces the malformed fence-2 example, "byte" → "character" throughout, atomicity residual replaced with the pest citation (S16), `mem` for `::describe`, D5.4 batched with the measured figure, red-first ceremony and doc-comment pins dropped, status flip removed from the batches, D4 collapses three sites into one helper.

### 6.2 Simplification pass — requirement-neutral cuts applied

1. The `#[ignore]`d `census` test (an engine test hard-coding consumer paths) — replaced by S21 + a PR-description paste.
2. D6.3's 7-day/nightly/cloud-deploy clauses and the log-grep stop condition — none produced evidence (S25).
3. The two-commit red-first ceremony and doc-comment offset pins — one commit; §4 is the record of the flip.
4. The sqlite backend for the `::describe` test — `mem` throughout.
5. D5.7's second, hand-copied doc pin — `types.cozo` through `doc_check` is the single pin.
6. The self-set status flip in the docs batch — status is the owner's act.
7. The atomicity-cascade fallback (`raw_string_inner = ${…}`) — documented pest semantics, cited in S16.
8. D1's "one-shot verification" listing — a duplicate of five §4 rows; replaced with a pointer.
9. B1/B2 merged into one batch (follows from cut 3) and D4's "convert one, audit two" replaced by one helper — no audit note to maintain.

---

## 7. Batches = review units

B1 is one `mnestic` PR with one commit; B2 spans `mnestic` (`CHANGELOG-FORK.md`), `mnestic-site` and the overlay skill; B3 is a `mindgraph-rs` PR; B4 is the tag. Each row is reviewed on its own question. Status is set by the owner: APPROVED after this review; SHIPPED after the B4 tag.

| Batch | Contents | Seam | Review question |
|---|---|---|---|
| **B1 — Grammar + tests + warning** (PR A, one commit) | the two-token change at `cozoscript.pest:207-208` (D1); `cozo-core/tests/string_literals.rs` with D5.1–D5.4, D5.6–D5.7; the D5.5 module test in `parse/fts.rs`; D7 (`emit_once` helper + two emission sites); PR description carries the D6.1 census paste | `cozo-core/src/cozoscript.pest:207-208`, `cozo-core/tests/`, `parse/fts.rs:149` module, `parse/expr.rs:451`/`:509`, `runtime/diagnostics.rs` | Is the grammar diff exactly `{`→`${` and `*`→`+`, with `raw_string_inner` untouched? Do the assertions match D2/D3 exactly, including the C4 (`\`) and C5 (EOI) offsets? Is D7 ≤ ~25 lines, once per run, and value-neutral (I10)? |
| **B2 — Docs + migration note + doc pin** | `types/page.mdx:287-320` rewrite (example `[3,4]`→`[3,3]`, callout replaced, BMP-only sentence); new `types.cozo` example file (mnestic-docs skill step 4/5) and the `doc_check:30` guard widened to `*.pest`; `doc_check types.cozo` transcript in the PR; `CHANGELOG-FORK.md` Unreleased entry = D3's migration note verbatim plus the `parser.string_decoding_changed` row in the warnings table | `mnestic-site/…/types/page.mdx`, `.claude/skills/mnestic-docs/`, `CHANGELOG-FORK.md` | Does every class C1–C5/C9/C10/C12 (and the undetectable C3b) appear in the note with a workaround? Does `doc_check` pass against a freshly built harness? |
| **B3 — Consumer hygiene** | the three `is_in(node_type, […])` sites → one `$types` helper (D4); mindgraph-rs test from §4 last row | `mindgraph-rs/src/storage/cozo.rs:10520-10524`, `:10579-10583`, `:10651-10655` | Is every interpolated `type_list` gone, and is `is_in(node_type, $types)` the only reader? |
| **B4 — Tag** | B1–B2 merged; D6.2 transcripts recorded; `versions.toml`/`bump.py` minor bump to 0.18.0; tag per the release skill; the 0.19.0 removal of D7 is added to `CHANGELOG-FORK.md` Unreleased at tag time | `versions.toml`, release skill | Did the census (D6.1) find anything beyond `page.mdx:295`? Are the D6.2 transcripts in the PRs? Is the D7 removal entry banked? |

### 7.1 Residual risks carried into review

- **Wheel-vs-tree evidence.** Every **(run)** result came from the 0.10.1 PyPI wheel. The string/comment grammar and the three decoders are unchanged from `fork-base` to HEAD, and `pest` was already at the 2.8 floor, but the authoritative confirmation is B1's suite against the working tree; if any §4 "Today" cell disagrees, the table — not the tests — is what gets corrected.
- **User corpus is unknowable.** C1–C3/C10 silently change values in scripts we cannot see (PyPI/crates.io/npm consumers). D7 makes each such script self-report once in the user's own `::warnings` for one release; the migration note and a minor bump are the rest. **C3b is not detectable** by any per-literal check and is covered only by the note's grep hint — stated honestly.
- **D7 is a two-release commitment.** It ships in 0.18.0 and must be deleted in 0.19.0; the removal is banked in Unreleased at tag time (B4) so it cannot be forgotten. If the owner cuts D7, this risk and the previous one collapse into "note only".
- **BMP-only `\u` is a decision, not an accident.** R11 records the ≈20-line alternative; reversing it at review changes D2's table, the note, `page.mdx` and one D5.2 row, nothing else.
- **`InvalidEscapeSeqError` is dead code.** The grammar rejects unknown escapes first (S11). Harmless; noted so no one claims it as a user-facing diagnostic.
- **Upstream provenance.** The `raw_string`-first ordering and `_*` fence are inherited verbatim from upstream cozo (the rules are unchanged since `fork-base`); whether upstream intended zero-fence raw strings was not researched. The fix is offered upstream only after B4 — nothing here depends on that.
