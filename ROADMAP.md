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

## Current state

- `mnestic` 0.17.0 and `mnestic-rocks` 0.1.12 are the latest published releases.
- Atomic Parquet/Arrow import and candidate-aware FTS shipped in 0.17.0.
- Arrow export is not implemented and remains demand-gated.
- LangChain, LlamaIndex, LangGraph, and MCP integrations are published.

### Already shipped through 0.17.0

| Capability | Release | Boundary |
|---|---:|---|
| Atomic Parquet/Arrow copy-in | 0.17.0 | One local file into an existing non-`TxTime` relation; Arrow export remains demand-gated |
| Candidate-aware FTS | 0.17.0 | Exact primary-key allowlist before top-k; BM25 statistics remain corpus-global |

## Next release: 0.18.0

The owner selected #53 and #54 on September 5; reviewed implementations are pushed to `main`
in `ff3fa2d3`. The next authorized batch adds #55 and fixes the reproduced integer-FTS-boost
parser panic. These changes are not released.
Local evidence is recorded in the [implementation review](docs/reviews/2026-09-05-string-json.md).
No storage-format or bridge change is planned. Migration notes and consumer checks are part of
this release scope, because existing literal values and newly constructed JSON can change.

| Work | State | Remaining gate |
|---|---|---|
| [String-literal correctness #53](https://github.com/shuruheel/mnestic/issues/53) | Pushed to main; [reviewed contract](docs/specs/string-literals.md) | Hosted CI and release checks |
| [Canonical nested JSON #54](https://github.com/shuruheel/mnestic/issues/54) | Pushed to main; [reviewed contract](docs/specs/json-canonical.md) | Hosted CI and published-wheel acceptance |
| [FTS prefix normalization #55](https://github.com/shuruheel/mnestic/issues/55) | Implemented; [reviewed contract](docs/specs/fts-prefix.md) | Regression checks, hosted CI and release checks |
| [HNSW verification/repair #56](https://github.com/shuruheel/mnestic/issues/56) | Investigation only | Reproduce a current failure before committing repair; confirmed corruption overrides order |

Keep 0.18.0 focused on these correctness changes. The integer-boost fix is covered by the
new prefix suite. Consumer compatibility is tracked in core PR #80; site PR #3 remains
a draft for publication with the release. Broader HNSW, Arrow export and quantization
work does not enter this release without the evidence gates below.

[Columnar I/O #11](https://github.com/shuruheel/mnestic/issues/11) combines two different states:
import shipped in 0.17.0; export is unimplemented and demand-gated. Treat only export as remaining
scope. The tracker has not yet been split.

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
