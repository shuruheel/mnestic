# Spec — Exact-Phrase Full-Text Queries and Matched-Span Snippets: make a quoted string mean what it says, and return the part of the document that matched

_Created 2026-07-27. Status: **SIGNED 2026-07-27 — all five §10 decisions approved by the owner as recommended, on the strength of the §10 verification record; shipped in 0.14.0.** This is the spec for the exact-phrase and `snippet()`/`highlight()` items on the public [`ROADMAP.md`](../../ROADMAP.md). Grounded against the shipped FTS pipeline — citations are file:line in `cozo-core/src/`, gathered and spot-verified 2026-07-27 — and against the repo's release practice. The work was originally scoped for patch 0.13.2, then joined the security tranche in the next minor release, 0.14.0. Companion to [`fts-corpus-stats.md`](./fts-corpus-stats.md) (the 0.13.0 BM25 statistics fix this builds beside) and [`passthrough-streaming.md`](./passthrough-streaming.md) (the spec-discipline template)._

> **Anti-overbuild guardrails.** **Zero storage-format changes, zero grammar changes, zero new index configuration.** The grammar already parses a quoted phrase as a distinct production (`cozoscript.pest:286`); the postings already store per-occurrence byte offsets *and* token positions (`fts/indexing.rs:606-628`); the decode path already reads the offsets and currently discards them — the `from`/`to` fields of `PositionInfo` are literally commented out (`fts/indexing.rs:72-76, 227-233`). The work is: one new AST variant, one evaluator arm (a restriction of the shipped `NEAR` intersection, `fts/indexing.rs:303-345`), one additive option on the FTS search atom mirroring `bind_score` (`data/program.rs:1166`), and one pure scalar formatting function that never touches a tokenizer. Budget it as a **contained feature on entirely-shipped storage** — smaller than the 0.13.0 corpus-stats fix. Anything demanding a posting-format change, a new index kind, or a per-index configuration knob is out of scope and a sign this spec is being exceeded.

---

## 1. Why / what this buys

`"connection refused"` today is **silently an AND of two independent terms**. The quoted string parses as one `FtsLiteral` (`parse/fts.rs:86-129` — `build_phrase` maps `quoted_string | s_quoted_string | raw_string` to the same `core_text` as an unquoted word group, keeping no record that quotes were present), and query-side tokenization then splits it: `do_tokenize` turns a multi-token literal into `FtsExpr::And` (`fts/ast.rs:130-141`). A document with "connection" in paragraph 1 and "refused" in paragraph 40 matches, with no error and no hint. That is the 0.12.2 class of defect — **the engine accepts the standard spelling of a question and answers a different question** — in the subsystem the fork's own docs call its weakest.

The fix costs almost nothing at the storage layer because upstream already paid for it: every posting stores, per occurrence, the token's byte span *and* its ordinal position (`fts/indexing.rs:606-628` writes `froms`/`tos`/`positions`; `:200-243` decodes all three), and the `NEAR` evaluator already intersects position lists across terms (`:303-345`). Exact phrase is `NEAR` with ordered, exact-distance adjacency instead of unordered windowing.

The same stored spans buy the second item. For agentic memory, returning a 4k-token document when a 40-token window matched is the context-budget failure mode; the matched byte spans are what a `snippet()` needs, and they are already on disk, already decoded, and currently thrown away. This is also the engine-side half of the roadmap's still-open "token-budgeted MCP output" item.

## 2. Shipped baseline this builds against (verified 2026-07-27)

| Piece | Where | What transfers |
|---|---|---|
| Grammar: `fts_phrase = {(fts_phrase_group \| quoted_string \| s_quoted_string \| raw_string) ~ fts_prefix_marker? ~ fts_booster?}` — quoted-vs-bare is **already a parse-level distinction** | `cozoscript.pest:281-294` | verbatim — **no grammar change**; the distinction is discarded one function later and merely needs to stop being discarded |
| `build_phrase` — the discard site: all four kernels collapse to `FtsLiteral{value, is_prefix, booster}` with no quoted flag (the local `is_quoted` is a misnamed holder for the `*` prefix marker) | `parse/fts.rs:86-129` | the primary edit site: thread `was_quoted: bool` out of the kernel match |
| `do_tokenize`: a multi-token `Literal` becomes `FtsExpr::And` — the exact line where the phrase dies | `fts/ast.rs:130-141` | the second edit site: quoted + multi-token ⇒ `Phrase`, not `And` |
| Postings store per-occurrence `froms`(vals[0]) / `tos`(vals[1]) / `positions`(vals[2]) / doc length(vals[3]); decode reads all four; `PositionInfo.from/to` are **commented out** | `fts/indexing.rs:72-76, 200-243, 606-628` | verbatim — **the entire storage story is done**; uncomment two fields |
| `NEAR` evaluation: per-doc position-list intersection across literals, seeded from the first literal's scan | `fts/indexing.rs:303-345` | as the **pattern to restrict**: phrase = the same loop with ordered exact-offset matching (§3.3) — plus a bug fix that rides along (§7) |
| BM25 scoring consumes `position_info.len()` as term frequency | `fts/indexing.rs:255-262` | verbatim — phrase tf = anchor count, no scoring change (§5) |
| `FtsSearch { …, bind_score: Option<Symbol>, filter, span }` — the additive-option precedent | `data/program.rs:1154-1169` | as the **pattern to copy** for `bind_spans` (§6) |
| Tokenizers assign `position` per *source* token, pre-filter (`wrapping_add(1)` from `usize::MAX`); `StopWordFilter` skips tokens **without renumbering** — removed words leave position holes; `Stemmer` mutates `text`, keeps `position` | `fts/tokenizer/simple_tokenizer.rs:39`, `whitespace_tokenizer.rs:39`, `stop_word_filter/mod.rs:126-133`, `stemmer.rs:100-116` | verbatim — this is what makes gap-faithful phrase matching *correct* under stemming and stopwords with no special cases (§4) |
| `NGram` tokenizer: "the `position` is always 0" | `fts/tokenizer/ngram_tokenizer.rs:6-8,155` | the reason §4.3's gate exists — adjacency is meaningless on an ngram index |
| Index manifest carries the analyzer config to the query side (`FtsIndexManifest{tokenizer, filters}` on every `FtsSearch`) | `fts/mod.rs:31-46`, `data/program.rs:1157` | verbatim — the query-time `NGram` gate can see the tokenizer kind with no new plumbing |
| 0.13.0 index-search diagnostics: errors carry the code of the index kind that failed | `CHANGELOG-FORK.md` 0.13.0 | as the **pattern to copy** for §4.3's named errors |

**What does NOT transfer: the matching semantics.** No shipped code answers "at which anchors does this ordered token sequence occur." That predicate (§3.3), its tokenizer-interaction contract (§4), and the span-binding surface (§6) are the genuinely new work. Everything they stand on is shipped.

## 3. Design — phrase queries

### 3.1 Recognition (parse time)

`build_phrase` records whether the kernel was `quoted_string | s_quoted_string | raw_string` (vs a bare `fts_phrase_group`). The flag travels on `FtsLiteral` as `is_phrase: bool`. Nothing else at parse time changes; bare word groups keep today's semantics exactly.

### 3.2 AST (tokenize time)

In `do_tokenize` (`fts/ast.rs:130-141`), a literal now resolves three ways:

- **not quoted** → today's behavior, unchanged: 1 token ⇒ `Literal`, n tokens ⇒ `And` — byte-identical results for every currently-meaningful query;
- **quoted, 1 token after tokenization** → `Literal` (so `"fox"` and `fox` stay equivalent, including `is_prefix` interaction — `"word"*` keeps its current single-term prefix meaning);
- **quoted, n ≥ 2 tokens** → new variant `FtsExpr::Phrase(FtsPhrase { tokens: Vec<(FtsLiteral, u32)>, booster })`, where the `u32` is the token's **query-side position from the analyzer's own token stream** — not a dense renumbering. Stopword holes in the query are preserved (§4.1 explains why that is the point).

### 3.3 Matching (eval time)

A new arm in `fts_search_impl` beside `Near` (`fts/indexing.rs:303-345`), same shape: scan the first token's postings to seed `doc → positions`, then for each subsequent token intersect. The restriction relative to `NEAR`: with `q0` the first token's query position, doc `D` matches at **anchor** `p` iff for every `(tᵢ, qᵢ)`, `D` has `tᵢ` at exactly `p + (qᵢ − q0)`. Ordered, exact offsets — not `NEAR`'s unordered `|Δ| ≤ distance`. The arm's result per doc is the set of anchors, which is exactly the `doc → positions` shape the surrounding code already passes around.

### 3.4 Non-goals in v1 (each gets a named error, not silence)

- **Phrase-prefix** (`"connection refu"*`, ≥ 2 tokens): today this is *silently broken* — `FtsLiteral::tokenize` short-circuits on `is_prefix` (`fts/ast.rs:21-27`), so the whole quoted string is prefix-matched as one term against single-token posting keys and matches nothing, ever. v1 rejects it with a not-yet-supported error naming the workaround (`"connection refused" OR refu*`). An error is strictly better than a guaranteed-empty result.
- **Phrase inside `NEAR(...)`**: today a quoted string inside `NEAR` degrades to its bag of tokens. §3.3's anchor sets compose naturally into `NEAR`'s intersection later; v1 rejects, so the phrase-means-AND bug class is not preserved inside the one operator we didn't get to.

## 4. The tokenizer contract — what "exact phrase" honestly means

This section is the spec's hard part, and the shipped position semantics make it clean: **positions are source-ordinal and survive every filter** (§2, row 8). Three consequences, stated as user-facing contract:

### 4.1 Stopwords: holes match anything, symmetrically

With an English stopword filter, `"jumped over the lazy dog"` tokenizes (query side, same analyzer) to `jumped@0, over@1, lazy@3, dog@4` — position 2 is a hole, and the match predicate simply does not constrain it. A document reading "jumped over **that** lazy dog" matches. This is the standard contract (Lucene's behavior, for the same reason) and the alternative — renumbering positions after filtering — would make phrase matching *wrong* in the other direction, rejecting documents whose only difference is a removed stopword. Document it plainly: **a stopword slot in a phrase is a one-token wildcard.**

### 4.2 Stemming: phrases match stems, symmetrically

The stemmer rewrites `text` and keeps `position` (`stemmer.rs:100-116`), and both sides run the same analyzer (the manifest travels on the search atom, §2 row 10). `"connection refused"` therefore matches "connections refusing" if the index stems both pairs together. Same contract as today's term search; no special case.

### 4.3 Position-degenerate tokenizers: refuse loudly

On an `NGram`-tokenized index every posting is at position 0 (`ngram_tokenizer.rs:6-8,155`); adjacency is unfalsifiable and every multi-token phrase would match every document containing the tokens. A phrase query against such an index is a **query-time error** in the 0.13.0 index-diagnostics style, naming the index, its tokenizer, and the fact that term search still works. The gate reads `manifest.tokenizer` — no new plumbing. (`Raw` needs no gate: it emits one token, so a phrase against it can only arise as §3.2's single-token degeneration.) The `split_compound_words` filter emits `position_length > 1` tokens; v1 does not special-case it — matching stays position-exact — but the contract section of the user docs names it as an analyzer whose phrase semantics are approximate.

## 5. Scoring

`FtsExpr::Phrase` scores exactly as a `Literal` whose per-doc term frequency is the **anchor count** (§3.3), flowing into the shipped BM25 path unchanged (`fts/indexing.rs:255-262` consumes `position_info.len()`; the phrase arm passes anchors where positions went). No new score kind, no new parameters. A phrase is rarer than its terms, so per-doc tf drops and the corpus df used for IDF is the phrase's own matching-doc count — both fall out of the existing code shape rather than being designed.

## 6. Design — matched spans and snippets

Two pieces, split so that neither needs a tokenizer at read time:

### 6.1 `bind_spans` on the FTS search atom (index-driven; the recommended first ship)

An additive `bind_spans: Option<Symbol>` on `FtsSearch`, exactly parallel to `bind_score` (`data/program.rs:1166`), surfaced in the search-atom option list as `bind_spans: sp`. Per returned document it binds a list of `[from, to]` **byte offsets into the original indexed text**, one entry per matched occurrence — for a term, the token's own span; for a phrase, first token's `from` to last matched token's `to` at each anchor. The data source is the `froms`/`tos` the decode path already reads and drops (`fts/indexing.rs:227-233`): uncomment `PositionInfo.from/to`, keep them through the evaluator, emit them only when the binding is requested. Index-consistent under stemming by construction, because the offsets were recorded by the analyzer that built the index. Zero re-tokenization, zero storage change.

### 6.2 `snippet(text, spans, window)` — a pure formatting scalar

A scalar function taking the document text, a span list (from §6.1), and a character-budget window; returns the highest-density window(s) around matched spans, cut on `char` boundaries (the stored offsets are byte offsets — the implementation must round to UTF-8 boundaries, and a test must cover a multi-byte document). **It never tokenizes**, so there is no analyzer-mismatch failure mode — which is the reason this spec rejects the `highlight(text, query)` scalar shape that would have to guess the analyzer: on a stemmed index it silently misses the very matches the index found. A markup variant (`highlight`-style wrapping of spans) is the same function with a format argument, at the implementer's discretion.

## 7. A rider fix: `NEAR`'s first literal is scanned twice

The `NEAR` evaluator seeds from `l_it.next()` and then re-iterates `for lit_nxt in literals` **from the start** (`fts/indexing.rs:304-322`), so literal 0 is fetched twice and intersected against itself. Results are unchanged (self-distance 0 always survives), but every `NEAR` query pays one redundant full posting scan. The phrase arm is a copy of this loop; fix the skip in both. Rides in the same release as a Fixed entry.

## 8. Versioning, compatibility, migration note

**0.14.0 — a minor release**, because this work ultimately shipped with the security tranche and its opt-in/data-access changes. The FTS portion itself is not a Rust source break: `FtsSearch` is `pub(crate)`, `bind_spans` defaults absent, `snippet` is a new function, and no storage format moves in either direction — an index written by 0.14.0 opens under 0.13.0 and vice versa.

The migration note it owes (0.12.2 style — the break *is* the fix working): **a quoted multi-word FTS query changes meaning.** Anyone relying on `"connection refused"` meaning `connection AND refused` gets phrase semantics — strictly fewer, more relevant rows — and should drop the quotes to keep AND. The two named errors (§3.4) replace one silently-empty result and one silently-wrong one. `CHANGELOG-FORK.md` leads the entry with this, and both immutable readmes carry it per the release checklist.

## 9. Testing

- **Subset oracle** (the differential-suite pattern, `tests/factorize.rs`): for generated corpora and phrase queries, `phrase("a b")` results ⊆ `a AND b` results, with the difference exactly the non-adjacent docs a hand-check confirms; run on **sqlite** per the repo test-backend rule.
- **Contract fixtures** for each §4 clause: stopword-hole wildcard (matches and non-matches), stemmed phrase, ngram gate error, phrase-prefix error, `NEAR`-of-phrase error, single-token quoted ≡ unquoted, and byte-offset correctness on a multi-byte (CJK/emoji) document for §6.
- **`tests/spec_doc_validation.rs`** pins every ✓-marked listing this spec's user-facing docs gain, so the claims cannot rot.
- The §7 fix gets a scan-count regression test (posting-fetch counter or instrumented handle), not just a results test — the bug is invisible in results by construction.

## 10. Decisions required before build (sign here)

1. **Stopword-hole contract (§4.1)** — accept holes-as-wildcards (recommended; Lucene-consistent, and the only option the stored positions support without renumbering), or reject phrases containing stopwords with a named error?
2. **Ngram gate (§4.3)** — query-time error (recommended), or documented degradation to AND? An error is the spec's whole ethos; degradation recreates the bug being fixed.
3. **v1 rejections (§3.4)** — confirm phrase-prefix and phrase-in-NEAR ship as named errors, deferring their implementations without deferring their diagnostics.
4. **`snippet` shape (§6.2)** — confirm the spans-argument scalar (tokenizer-free) over `highlight(text, query)`; this is the one place this spec reverses an earlier informal recommendation, on the evidence that the offsets are already decoded and index-consistent.
5. **Release number** — originally approved for 0.13.2; shipped in 0.14.0 with the §8 migration note after the release bank expanded to include the security tranche.

### Verification record (2026-07-27, pre-sign-off)

The behavioral claims above were verified **empirically against the shipped 0.13.1 engine** (a scratch integration suite, sqlite backend, 4/4 passing — its assertions are re-created as §9's permanent fixtures during implementation): a quoted multi-word query matches a non-adjacent document (`"hello world"` matches "hello wide world"); a quoted multi-word prefix query (`"hello wor"*`) returns zero rows silently; a quoted phrase inside `NEAR` imposes no adjacency; and with `Stopwords('en')`, `NEAR/1(alpha beta)` fails while `NEAR/2(alpha beta)` matches "alpha the beta" — proving positions preserve stopword holes (note when building §9 fixtures: pick non-stopword terms; "over" is in the EN list and will vaporize a query-side literal).

Prior art was checked per decision: **(1)** Lucene's `StopFilter` preserves position increments and `PhraseQuery` matches across the resulting gaps; the renumbering alternative (`enablePositionIncrements=false`) was *removed* in Lucene 4.4 (LUCENE-4963) for producing broken token streams. **(2)** Lucene/Solr answer a phrase query on a position-less field with a hard `IllegalStateException` — the canonical field report being an ngram field — and SQLite FTS5 has a known snippet/highlight misbehavior thread under its trigram tokenizer; erroring is the established practice. **(3)** The deferred phrase-prefix has a settled eventual design (Elasticsearch `match_phrase_prefix` / Lucene `MultiPhrasePrefixQuery`: last term prefix-expanded, expansion-bounded), so the v1 named error forecloses nothing. **(4)** Lucene's `UnifiedHighlighter` prefers offset sources in the order postings → term vectors → re-analysis, postings being fastest and smallest — this spec's `bind_spans` is that first-preference mode natively.
