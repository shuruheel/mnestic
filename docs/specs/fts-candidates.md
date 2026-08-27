# Candidate-aware full-text search

Status: implemented in the unreleased working tree on 2026-08-27.

## Contract

An FTS atom may restrict eligible documents by base-relation primary key:

```cozoscript
?[id, score] := ~doc:fts{
    id |
    query: $query,
    k: 20,
    bind_score: score,
    candidates: $allowed
}
```

`candidates` must constant-fold to a list. For a one-column key, each list
element is the key value. For a composite key, each element is a list with the
same arity and column order as the base relation key. Every element is coerced
through the declared key type before membership is evaluated. Invalid values,
wrong arity, non-list input, and nonconstant expressions fail loudly. An empty
list returns no rows.

The normalized set is stored as `Arc<FxHashSet<Tuple>>`, so it is constructed
once per compiled search atom and shared by every parent tuple that drives the
operator.

## Ranking semantics

Candidates change eligibility, not scoring:

- BM25 document count `N` and average document length remain index-global.
- A literal records its unrestricted posting count before candidate filtering;
  that count remains the term document frequency used by BM25 and TF-IDF.
- Phrase and NEAR document frequency is captured after the unrestricted
  positional intersection and before candidate filtering.
- AND, OR, and NOT compose over already restricted result maps.
- Sorting and `k` truncation operate on the eligible result set.
- The existing `filter:` option remains independent and continues to run on
  fetched base tuples after ranking.

Therefore, at a fixed corpus state, a `(query, document)` pair has the same
score with and without `candidates:`. Corpus-global statistics deliberately
mean that writes elsewhere in a shared index may still change scores. This
feature does not provide per-tenant statistical isolation.

## Performance boundary

The option eliminates scoring, result-map growth, sorting, and base-relation
fetches for noncandidate literal hits. The current posting layout still scans
the query term's posting range to obtain global document frequency. This makes
cost proportional to matching postings plus eligible result processing, not
strictly proportional only to the allowlist size.

Adaptive lead-side point probes and cached bitmap scopes are deferred until
measurements show posting enumeration dominates. Callers must bound allowlist
size; MindGraph uses its shared 20,000-node exact-scope threshold.

## Compatibility

Queries without `candidates:` follow the prior code path and preserve existing
ranking. Older engines reject the unknown option instead of silently falling
back to global top-k behavior. The option is engine-neutral: it knows only
relations, primary keys, postings, and scores.

## Acceptance evidence

`cozo-core/tests/fts_candidates.rs` covers:

1. a candidate ranked below global `k` remains retrievable;
2. AND, OR, NOT, and NEAR interactions;
3. score invariance with `bind_score` and `filter:`;
4. composite keys and Int-to-Float key coercion;
5. empty, malformed, nonconstant, and multi-parent cases.
