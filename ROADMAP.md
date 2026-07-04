# mnestic — Roadmap

mnestic is a maintained, independently-developed fork of [CozoDB](https://github.com/cozodb/cozo) — a transactional relational–graph–vector database that uses Datalog — focused on being a first-class **substrate for agentic memory**. See [`FORK.md`](./FORK.md) for provenance and attribution (all credit for the original design belongs to Ziyang Hu and the Cozo Project Authors), [`CHANGELOG-FORK.md`](./CHANGELOG-FORK.md) for everything shipped since the fork point, and the design specs under [`docs/specs/`](docs/specs/) for the deeper engineering plans behind the items here.

This document is the **public, forward-looking roadmap**: where the project is going and how to help.

## Our commitment

The single most important promise of this fork is that it is **actively maintained.** Upstream Cozo has been dormant since late 2024; mnestic exists to carry the engine forward for everyone building on it. Concretely that means a steady release cadence, responsiveness to issues, and a clear public direction — this doc.

## North star

Make the engine the best **substrate for agentic memory**: in *one embedded engine*, deliver

- **Hybrid retrieval** — vector (HNSW) + keyword (BM25) + graph traversal, fused in a single call;
- **Temporally-correct knowledge** — query memory as-of any point in time, and bi-temporally ("what did we believe *when*" — shipped in 0.10.0);
- **Incremental index maintenance** — upserts that keep vector/FTS indexes current without full rebuilds or long write locks;
- **Fast point-lookups co-located with semantic search**; and
- **The operational tooling** to run long-lived graph memory in production.

Every item is judged against that goal — and described as a general database mechanism, not in terms of any specific application.

## What's already shipped (through 0.10.0)

The agentic-memory *retrieval* foundation is largely in place. Highlights (see [`CHANGELOG-FORK.md`](./CHANGELOG-FORK.md) for detail):

- **Bi-temporality — system-versioned (`TxTime`) relations** (0.10.0, the marquee feature): an engine-assigned transaction-time axis alongside Cozo's valid-time — commit-clock stamping, current-state reads by default, time-travel `@ (vt: ..., tt: ...)` and `:as_of`, existence-checking writes, `::history` / `::history_gc` (persisted floor) / `::evict` (audited hard deletion), and `:reconcile` declarative belief revision. "What did we believe at time *T* about period *Y*" — in-engine, in an embedded database. Spec: [`docs/specs/bitemporality.md`](docs/specs/bitemporality.md).
- **Custom aggregates + top-k proofs** (0.10.0): `register_custom_aggr` for domain-specific absorptive combines in recursive rules, and `min_cost_k([payload, cost], k)` — a bounded-meet aggregate returning the k best derivations per answer with their evidence (Scallop-style approximate top-k). Spec: [`docs/specs/provenance-semirings.md`](docs/specs/provenance-semirings.md).
- **Read-only Cypher query surface** (alpha; behind the off-by-default `cypher` Cargo feature) — translates a subset of openCypher (`MATCH` / `WHERE` / `RETURN` with `DISTINCT` & aggregates / `ORDER BY` / `SKIP` / `LIMIT`; true bag semantics; null-aware `WHERE`; edge-isomorphism) to CozoScript, so the engine is easy to evaluate without learning Datalog first. Read interop only; Datalog stays the native, full-power language. API: `run_cypher` / `cypher_to_script`. The PyPI wheel ships without it for now (build `--features cypher`). Spec: [`docs/specs/cypher-read.md`](docs/specs/cypher-read.md).
- **One-call hybrid retrieval** (`HybridSearch`) with **Reciprocal Rank Fusion** and **MMR** diversity reranking.
- **Native 3-way fused recall** — vector + full-text + *k*-hop graph proximity fused in a single query (typed `GraphLeg`).
- **Okapi BM25 full-text scoring** with `k1`/`b` tuning, multi-term OR-summation, and O(1) average-document-length.
- **Per-leg fusion detail** — reconstruct exactly why each result was retrieved.
- **LangChain & LlamaIndex integrations** on PyPI (`mnestic`, `langchain-mnestic`, `llama-index-vector-stores-mnestic`).
- **Fast, non-blocking HNSW index builds** — a flat in-RAM parallel build (~15× faster on a 40k×384 corpus) that doesn't stall readers.
- **Snapshot read path** + batched neighbour fetch (`multi_get`) for read-only queries.
- **Corruption resilience** — `::repair_corrupt` and tolerant index builds, so one bad row never makes a database unopenable.
- **Planner & DX fixes** — equality-pushdown keyed lookups (~28× on point queries), keyword-prefixed identifier parsing, ULID functions.

## What's next

Tiered by value and how ready each item is. This is direction, not dated commitments.

### Near-term — correctness, cadence & developer experience

- **Steady release cadence** with clear migration notes.
- **Closing long-open upstream issues** under active maintenance — e.g. Sled backend `del()` correctness, SQLite-backend performance (prepared statements / `WITHOUT ROWID`), and modeling ergonomics for tree-shaped / JSON-LD data.
- **Better onboarding** — clearer binder errors and worked examples, lowering the "modeling my data in Datalog" learning curve.

### The differentiators we're building

- **Extending the Cypher-read surface** — variable-length paths, `OPTIONAL MATCH`, `WITH`, and undirected relationships (today these return explicit not-yet-supported errors). Spec: [`docs/specs/cypher-read.md`](docs/specs/cypher-read.md).
- **Stored / named queries** — reusable, parameterized retrieval rules; also the substrate for a future compiled-plan cache.
- **A first-class ULID type** and sortable auto-keys (the scalar functions already ship; the type does not yet).
- **An official schema-migration tool** — versioned schema, diff, and rollback.
- **`LOAD FROM` Parquet/Arrow** + zero-copy Arrow export, for clean handoff with Python/Rust data pipelines.

### Performance at scale (evidence-gated)

Pursued when a real workload demonstrates the need, always with before/after measurements (the project's baseline-first rule):

- Compiled-plan / prepared-statement caching for high-frequency point reads.
- Selectivity-tiered filtered vector search (efficient metadata-filtered ANN).
- Full-text scale headroom (compact posting-list storage + top-*k* pruning).
- Tunable/weighted fusion and graph-leg improvements, gated on retrieval benchmarks.
- Vector quantization with float rescore, gated on corpus-scale evidence.

## Non-goals (deliberate scope)

mnestic is an **embedded, single-node** engine specialized for agentic memory. To stay excellent at that, the following are intentionally out of scope:

- **Distributed clustering / replication / consensus** inside the engine.
- **Multi-model breadth** (becoming a general document/time-series/KV store with many query languages) — the opinionated graph+vector+FTS focus is the point.
- **Data federation / virtualization** over external warehouses or lakehouses (mapping a schema onto external sources without copying). Agentic memory is copy-and-transform; that's a different kind of system.
- **Cypher *write* semantics** (read interop already ships as an alpha feature; full/write Cypher is not on the path).
- **CRDT multi-device sync** and **browser/WASM persistence** — real demand, but off this project's focus.

These can be revisited if a concrete agentic-memory need ever forces them, but they are not on the path today.

## How to contribute

Contributions are very welcome — mnestic is meant to serve the whole Cozo/Kùzu-refugee community, not just its primary downstream consumer.

- **Good first issues** are labeled in the tracker; the "near-term" items above are the best on-ramps.
- **Performance work must be baseline-first** — include before/after numbers; the repo has criterion benches to build on.
- **Tests:** keep the inherited engine tests green (they encode upstream semantics), and use the **SQLite** backend for any planner/stored-relation test (the in-memory backend uses a different join operator). See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for the test-backend conventions.
- **Licensing:** mnestic is MPL-2.0. Preserve the original `Copyright … The Cozo Project Authors` headers on any file you modify.

If you're considering a larger feature (for example extending the Cypher-read surface, or building on the now-shipped bi-temporality — GC policy, audit tooling, benchmarks), open an issue to discuss the design first.

## Releases & versioning

- **SemVer on the 0.x line** — patch for fixes, minor for non-drop-in behavior/feature changes.
- Releases are published to **crates.io** (`mnestic`, `mnestic-rocks`) and **PyPI** (`mnestic`), with divergences recorded in [`CHANGELOG-FORK.md`](./CHANGELOG-FORK.md).
- Work is banked and released on a regular rhythm with migration notes — steady stewardship over churn.

## Relationship to MindGraph

mnestic is developed alongside, and consumed by, [MindGraph](https://crates.io/crates/mindgraph), a typed cognitive knowledge-graph library built on top of it. MindGraph is the engine's most demanding user and drives much of its hardening — but mnestic is a general-purpose database in its own right, and its roadmap is set to serve the broader community of developers building local-first and embedded AI memory.
