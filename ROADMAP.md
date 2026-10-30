# mnestic — Roadmap

mnestic is an actively maintained fork of [CozoDB](https://github.com/cozodb/cozo), focused on
being the best embedded substrate for agentic memory. This is the public, forward-looking roadmap.
Shipped detail lives in [`CHANGELOG-FORK.md`](CHANGELOG-FORK.md); implementation contracts live in
[`docs/specs/`](docs/specs/); provenance and attribution live in [`FORK.md`](FORK.md).

## Commitment and north star

Mnestic provides steady maintenance, clear releases, and a focused path for developers building
long-lived local or embedded memory. Its defining combination is:

- vector, full-text, and graph retrieval in one engine;
- valid-time and transaction-time history;
- recursive inference and proof-carrying results;
- incremental indexes and bounded operations; and
- trustworthy production maintenance without a separate database service.

Every new feature must fit that agentic-memory wedge as a general engine mechanism.

### Beyond recall — strategic clarification, September 7, 2026

Long-lived applications also need to revise evidence, track changing work and
coordinate participants. Mnestic's role is to make general storage, history,
transactions, recursive queries and bounded retrieval dependable for those uses.
Application ontologies, hypotheses, projects/tasks, approvals and model-curation
policies belong in consumers such as MindGraph, not inside the engine.

Prefer verified use of existing primitives to new mechanisms. Bring a minimal,
domain-neutral reproducer and explicit correctness/resource bounds when a real
workflow exposes a gap. Keep engine correctness and resource measurements separate
from end-to-end reasoning, collaboration or token-saving claims. Markdown-backed
applications remain legitimate alternatives; an engine feature alone does not
establish their inferiority.

This clarification creates no new release commitment or benchmark campaign. The
maintenance priorities, 0.19 selection rules and demand gates below are unchanged;
release/issue descriptions retain their September 6 reconciliation date.

## Current state

- `mnestic` 0.18.0 and `mnestic-rocks` 0.1.12 are the latest published releases.
- Atomic Parquet/Arrow import and candidate-aware FTS shipped in 0.17.0.
- Arrow export is not implemented and remains demand-gated.
- LangChain, LlamaIndex, LangGraph, and MCP integrations are published.

### Already shipped through 0.18.0

| Capability | Release | Boundary |
|---|---:|---|
| String-literal correctness (#53) | 0.18.0 | Quoted escapes decode and raw strings preserve contents; audit scripts and stored-query bodies |
| Canonical nested JSON (#54) | 0.18.0 | UUID/base64/infinity forms match top-level JSON; stored JSON is not rewritten |
| FTS prefix normalization (#55) | 0.18.0 | Lowercase and ASCII folding apply to prefixes of indexed terms; integer boosts no longer panic |
| Atomic Parquet/Arrow copy-in | 0.17.0 | One local file into an existing non-`TxTime` relation; Arrow export remains demand-gated |
| Candidate-aware FTS | 0.17.0 | Exact primary-key allowlist before top-k; BM25 statistics remain corpus-global |

## After 0.18.0

0.18.0 ships the reviewed [string-literal](docs/specs/string-literals.md),
[canonical JSON](docs/specs/json-canonical.md), and
[FTS prefix](docs/specs/fts-prefix.md) contracts, plus the integer-FTS-boost panic fix.
The crate and Python release tags identify commit `1d13aedc`; storage format and bridge
version are unchanged. Read the [migration notes](CHANGELOG-FORK.md) before upgrading:
existing script literals and newly constructed JSON can change.

Implementation evidence remains in the [string/JSON review](docs/reviews/2026-09-05-string-json.md)
and [FTS review](docs/reviews/2026-09-05-fts-prefix.md); these are historical implementation
snapshots, separate from release evidence. Consumer compatibility changes are merged in core
PR #80; core adoption of `v0.18.0` merged in
[PR #81](https://github.com/shuruheel/mindgraph-rs/pull/81). The
[documentation update](https://github.com/shuruheel/mnestic-site/pull/3) is deployed.
These are separately verified delivery states; no cloud deployment is implied.

### 0.19 planning

The [0.19 release plan](docs/plans/0.19-planning.md), updated October 30, selects
already-merged governed native Rust multi-transactions, responsive `::warnings`,
the promised migration-warning removal, and the E1–E3 measurement/guidance deliverables.
**November 12, 2026 is the provisional release target; scope freezes November 5.**
Confirm the target after the first measurement pass. Correctness and candidate-package
validation gate publication; November 19 is the fallback checkpoint with scope frozen.
A new filtered-vector mechanism remains conditional on evidence and review before freeze.
See the [measurement protocol](docs/plans/0.19-measurement-protocol.md) and
[filtered-vector investigation](docs/plans/0.19-filtered-vector-investigation.md).

| Work | State | Gate |
|---|---|---|
| Governed native Rust multi-transactions and responsive `::warnings` | Merged; selected for 0.19, unreleased | Validate candidate against the transaction contract, including cancellation/commit ambiguity and host worker ownership |
| Remove temporary `parser.string_decoding_changed` warning | Scheduled for 0.19.0 | Keep the migration guidance after removing the diagnostic |
| Remeasure the public hybrid-recall benchmark on 0.18 | Selected October 27 — measurement | Published numbers are from May on an older build; refresh `RESULTS.md` from the current build, add a filtered-query workload (type-restricted vector plus FTS) and a read-your-writes latency column |
| Filtered vector search for selective predicates | Selected October 27 — investigation | Evidence: a consumer's type-restricted vector query over a rare type returned nothing because it post-filters a global ANN prefix. Verify and measure the inherited HNSW `filter:` path on the new workload first; add `candidates:` on HNSW (mirroring 0.17's candidate-aware FTS) only if the filter path cannot meet a bounded-widening contract with resource bounds |
| Parse/plan cost of large vector literals and repeated query text | Selected October 27 — measurement | Consumers splice 1,000-plus-float vectors into script text per request; parameter-binding guidance first, a plan cache only on a measured high-frequency workload (existing gate) |
| [HNSW integrity #56](https://github.com/shuruheel/mnestic/issues/56) | E4 investigation; verifier/repair not committed to 0.19 | Failure fixtures and verified invariants before a read-only verifier; reproduce a current failure before committing repair |
| [Temporary-relation diagnostics #12](https://github.com/shuruheel/mnestic/issues/12) | Deferred behind the items above | Agree warning behavior and preserve legitimate transaction-scoped relations |
| [SQLite WITHOUT ROWID #6](https://github.com/shuruheel/mnestic/issues/6) | Deferred behind the items above | Demonstrate benefit across representative row sizes and workloads; preserve existing databases |

Broader HNSW, Arrow export and quantization work remains subject to the evidence gates below.

### Measurement first — October 27, 2026

The next engine work is small and measurement-led. Consumers building retrieval on mnestic
report end-to-end latencies an order of magnitude above the engine's measured fused
hybrid-recall time, so the engine is not the bottleneck today and does not earn a feature
release on that basis. Three items are selected above: a fresh benchmark on the current
build, a bounded investigation of filtered vector search, and a measurement of vector-literal
parse cost. Storage-layout and ergonomics proposals (#6, #8, #10, #11, #12, #19, #20, #57)
keep their demand gates and sit behind those three. Scope boundaries below are unchanged.

[Arrow export #11](https://github.com/shuruheel/mnestic/issues/11) tracks the unimplemented,
demand-gated export work. Its import half shipped in 0.17.0 and is retained as completed history.

## Adoption work

- Verify clean installation and first successful use of published integrations.
- Improve discovery, examples, and error quality before adding another adapter.
- Accept a framework integration only when it exposes on-wedge capabilities and has a credible
  distribution path.
- Remeasure public benchmark claims on the current engine before reusing them.

## Evidence-gated work

| Area | Activation evidence |
|---|---|
| Arrow export | A released-import user names an Arrow-native or zero-copy workflow |
| HNSW long-run prevention | Measured degradation after churn |
| Plan cache | Repeated planning cost on a real high-frequency workload |
| Filtered-vector redesign | A measured low-selectivity failure beyond bounded widening |
| FTS scale work | User-visible latency or documented corpus-scale pressure |
| Phrase-prefix / phrases inside NEAR | Concrete demand beyond current named errors and workarounds; agree matching and resource-limit contracts |
| Vector quantization | Corpus-scale storage pressure plus accepted recall bounds |
| Shared-cache expansion | Multi-instance memory evidence |
| Extended Cypher read | Observed evaluator friction |
| Additional query-language features | Concrete agentic-memory demand and a signed compatibility contract |

Correctness defects, storage contract tests, operational visibility, backup ergonomics, schema
migrations, and actionable errors remain valid maintenance or contributor work. The issue tracker
owns item-level status.

## Scope boundaries

Mnestic will not pursue:

- federation, virtualization, lakehouse breadth, or an extension marketplace;
- document/KV/time-series multi-model breadth or multiple write languages;
- distributed clustering or consensus inside the embedded engine;
- Cypher writes, CRDT sync, or browser/WASM persistence;
- native RDF/triple storage, SPARQL, or OWL reasoning;
- worst-case-optimal joins or competitive cyclic-subgraph matching; or
- a SOTA FTS rewrite or embedded Tantivy.

Boundary readers, read-only interoperability, and targeted general primitives remain valid when
they preserve the relational/Datalog core.

## Contributing and releases

- Discuss larger features in an issue before implementation.
- Make performance work baseline-first and keep inherited semantics green.
- Use the SQLite backend for planner and stored-relation tests; see
  [`CONTRIBUTING.md`](CONTRIBUTING.md).
- Preserve MPL-2.0 and inherited Cozo attribution.
- Publish `mnestic-rocks` before `mnestic` whenever the bridge changes.
- Treat roadmap text as intent; releases, registries, source, tests, and CI are delivery evidence.

Mnestic is developed alongside MindGraph, its most demanding consumer, but remains an independent
engine for the broader embedded and local-first agent-memory community.
