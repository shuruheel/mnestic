# Single-term FTS prefix normalization (#55)

Status: released in Mnestic 0.18.0 (commit `1d13aedc`). Reviewed September 5, 2026.

A trailing `*` searches prefixes of indexed terms. Before 0.18, the prefix bypassed all
filters: `Di*` missed `Diwank` on a `Simple + Lowercase` index, while `di*` and exact
`Diwank` matched. This is a query-side fix; existing indexes do not need rebuilding.

## Contract

Treat the incomplete prefix as one raw token. Apply configured `Lowercase` (`LowerCase`)
and `AsciiFolding` filters in their configured order, using the same implementations as
indexing. Do not apply the configured tokenizer, stemming, stopword removal, compound-word
splitting, `AlphaNumOnly`, or `RemoveLong` to an incomplete term. These stages depend on
complete words or can discard/split a prefix. A future filter must explicitly opt in to
prefix normalization; its default is to leave the prefix unchanged.

Matching is against indexed terms, including stems if the index stems words. For example,
`run*` can match the indexed stem `run` of `running`; `running*` need not match that stem.
A stopword prefix such as `the*` can match `theory` even when the complete word `the` is
excluded. A Raw or NGram index still receives one normalized prefix, never an AND of grams.
This does not promise linguistic completion or substring search.

Quoted prefixes that the configured tokenizer splits into multiple tokens remain unsupported
with the phrase-prefix diagnostic (#19), including when later filters would erase a token.
The same guard applies inside NEAR. Bare single-term prefixes, single-token quoted prefixes,
boosts, Boolean composition, and NEAR operands share one normalization path. An empty prefix
must never open an unbounded posting scan. Leading wildcards remain rejected by the parser.

The existing bounded lexicographic posting-key range scan and candidate filtering remain
unchanged. Exact terms and exact phrases retain their full analyzer pipeline.

## Acceptance

- ASCII and Unicode lowercase; folding and filter order; no-filter case sensitivity.
- Stemmer, Stopwords and RemoveLong do not rewrite/discard incomplete prefixes.
- Raw, Simple and NGram configurations; quoted and NEAR prefixes and boosts.
- Empty quoted prefixes produce no tokens; reject leading wildcards and multi-token phrase-prefix forms.
- Candidate restrictions, update/delete maintenance, and SQLite reopen behavior.
- Existing FTS phrase, proximity, scoring and candidate regression suites pass.

## Adjacent reproduced fix

The acceptance test `DI*^3` exposed an inherited panic: the grammar returns `pos_int`
but the booster parser matched `int`. Handle the emitted rule and prove integer/decimal
boost equivalence for both prefix and exact terms. Out-of-range integers remain errors.
