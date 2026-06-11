# `mnestic`

> **mnestic** is an independently maintained fork of [CozoDB](https://github.com/cozodb/cozo),
> a transactional relational-graph-vector database that uses Datalog for queries —
> "the hippocampus for AI." This fork continues the project as a substrate for
> **agentic memory**, with performance, correctness, and operational fixes on top
> of upstream `481af05` (the last upstream commit, 2024-12-04).
>
> mnestic is **not** the official CozoDB and is not affiliated with or endorsed by
> its original authors. All credit for the original design belongs to Ziyang Hu and
> the Cozo Project Authors. See [`FORK.md`](FORK.md) for provenance and licensing,
> and [`CHANGELOG-FORK.md`](CHANGELOG-FORK.md) for what diverges from upstream.

## What mnestic adds over CozoDB

Highlights (full detail in [`CHANGELOG-FORK.md`](CHANGELOG-FORK.md)):

**Unreleased (on `main`, ships in 0.8.5)**

- **Flat in-RAM parallel index builds** — `::hnsw create` now constructs the
  graph in flat, integer-indexed memory (contiguous vector slab + per-node
  adjacency arrays, the hnswlib/pgvector layout) with parallel insertion under
  per-node locks, then serialises once into the unchanged on-disk format.
  Measured on the 40k × 384-dim RocksDB benchmark corpus: **294 s → 19 s**
  synthetic, **89.1 s → 8.1 s** with real embeddings, recall@10 unchanged
  (0.8838 → 0.8824). Decomposition: 3.2× from the flat layout (serial), ~5×
  from parallel insertion. `::fts create` drops a redundant second
  tokenisation pass and tokenises in parallel (~2× on short docs; the win
  scales with document length). Same search path, same incremental
  maintenance, still non-blocking. `MNESTIC_INDEX_BUILD_THREADS=1` restores
  serial insertion order.

**0.8.4**

- **Per-leg fusion detail** — `ReciprocalRankFusion(..., detailed: true)` /
  `HybridSearch::detailed` return one row per *(item, contributing leg)*:
  `[item, fused_score, list_id, leg_rank, leg_score]`. The fused score
  reconstructs exactly as `Σ 1/(k + leg_rank)` — the substrate for
  "why was this retrieved" surfaces. Python binding: `detailed=True`.
- **Concurrency fix** — 0.8.3's durable `avgdl` counter (one shared key written
  in every document transaction) made concurrent writers to FTS-indexed
  relations contend on a single RocksDB row lock and could lose counter
  updates. Doc-stats are now process-cached and scan-seeded; per-query `avgdl`
  stays O(1) and BM25 scores are unchanged.
- **Python on PyPI** — `pip install mnestic` (abi3 wheels: Linux x86_64/aarch64,
  macOS Intel/arm64, Windows), plus `langchain-mnestic` and
  `llama-index-vector-stores-mnestic` adapters.

**0.8.3**

- **Native 3-way fused recall** — `hybrid_search` now fuses a graph-proximity leg
  *in-engine* alongside vector (HNSW) and full-text (FTS). A typed `GraphLeg`
  generates a recursive bounded shortest-path rule (k-hop, `min(dist)` scoring) and
  folds it into the same RRF, so one call returns the vector+FTS+graph ranking — a
  capability no other embedded engine here offers. Measured **41.55 ms p50**, ~4×
  faster than the hand-decomposed three-query path.
- **BM25-correct FTS, with O(1) `avgdl`** — the default `::fts` scorer is now Okapi
  **`bm25`** (term-frequency saturation `k1` + document-length normalization `b`,
  both tunable; `OR` sums per-term contributions). Average document length is an O(1)
  read rather than a per-query index scan (durable-counter design replaced by a
  process-level cache in 0.8.4). Measured:
  fused recall **0.75 → 0.954** (parity with DuckDB / SQLite) at no net latency cost
  (decomposed p50 927 → 175 ms, cold p99 2,900 → 258 ms). **Heads-up:** this changes
  the default FTS score kind (a behaviour change); `tf`/`tf_idf` stay selectable for
  byte-identical upstream scoring.

**0.8.2**

- **Non-blocking HNSW index builds** — `::hnsw create` no longer holds the base
  relation's write lock during graph construction, so concurrent reads no longer
  stall for the whole build (previously 10–20+ min in production). The graph is
  built off-lock under a snapshot and bulk-published via `SstFileWriter`/
  `IngestExternalFile`; mutations during the build are reconciled under a brief
  final lock. Measured: 90,507 reads completed (slowest 0.8 ms) during a ~5.6 s
  40k-vector build that would previously have blocked them all. RocksDB only.

**0.8.1**

- **One-call hybrid retrieval** — `DbInstance::hybrid_search` runs HNSW + FTS
  (+ optional graph traversal), fuses with RRF, and optionally diversifies with
  MMR in a single typed call (previously ~7 hand-written Datalog rules).
- **HNSW index build ~3× faster** — the build no longer round-trips the whole
  graph through the transaction's write-batch overlay (20k × 128: 135s → 43.6s,
  measured release); built graph is byte-identical.
- **`mnestic-rocks`** — the C++/RocksDB bridge is now a maintained fork (importable
  name stays `cozorocks`), unblocking future bridge-level work.
- Blocking clippy CI gate; `document-features` future-incompat warning cleared.

**0.8.0 — fixes**

- **Equality pushdown** — `*rel[k, ..], k == <value>` now compiles to a keyed
  `stored_prefix_join` instead of a full scan (**~28–29× faster** single-row
  primary-key lookups, measured at 5k rows). Numeric equalities keep cross-type
  `op_eq` semantics.
- **Parser fix** — identifiers that start with a keyword literal
  (`nullable_column`, `trueValue`, `falsey`) now parse correctly (upstream #281).
- **Unreleased upstream fixes for free** — the fork point is 30 commits ahead of
  the published 0.7.6, including the `stored_prefix_join` correctness fix.
- `env_logger` moved to a dev-dependency for a slimmer dependency graph
  (upstream #287).

**0.8.0 — new: hybrid retrieval for agentic memory** (Datalog-composable fixed rules)

- `ReciprocalRankFusion` (alias `RRF`) — fuse vector (HNSW) + full-text (FTS) +
  graph-traversal result lists into one ranking.
- `MaximalMarginalRelevance` (alias `MMR`) — diversity-aware reranking that avoids
  near-duplicate recalls.
- `rand_ulid()` / `ulid_timestamp()` — lexicographically-sortable identifiers for
  time-ordered scans (upstream #296).

---

The remainder of this README is upstream CozoDB's documentation. The query
language (CozoScript / Datalog) and engine semantics are unchanged unless noted
in `CHANGELOG-FORK.md`.

### Table of contents

1. [Introduction](#Introduction)
2. [Getting started](#Getting-started)
3. [Install](#Install)
4. [Architecture](#Architecture)
5. [Status of the project](#Status-of-the-project)
6. [Links](#Links)
7. [Licensing and contributing](#Licensing-and-contributing)

## Introduction

CozoDB is a general-purpose, transactional, relational database
that uses **Datalog** for query, is **embeddable** but can also handle huge amounts of data and concurrency,
and focuses on **graph** data and algorithms.
It supports **time travel** and it is **performant**!

### What does _embeddable_ mean here?

A database is almost surely embedded
if you can use it on a phone which _never_ connects to any network
(this situation is not as unusual as you might think). SQLite is embedded. MySQL/Postgres/Oracle are client-server.

> A database is _embedded_ if it runs in the same process as your main program.
> This is in contradistinction to _client-server_ databases, where your program connects to
> a database server (maybe running on a separate machine) via a client library. Embedded databases
> generally require no setup and can be used in a much wider range of environments.
>
> We say CozoDB is _embeddable_ instead of _embedded_ since you can also use it in client-server
> mode, which can make better use of server resources and allow much more concurrency than
> in embedded mode.

### Why _graphs_?

Because data are inherently interconnected. Most insights about data can only be obtained if
you take this interconnectedness into account.

> Most existing _graph_ databases start by requiring you to shoehorn your data into the labelled-property graph model.
> We don't go this route because we think the traditional relational model is much easier to work with for
> storing data, much more versatile, and can deal with graph data just fine. Even more importantly,
> the most piercing insights about data usually come from graph structures _implicit_ several levels deep
> in your data. The relational model, being an _algebra_, can deal with it just fine. The property graph model,
> not so much, since that model is not very composable.

### What is so cool about _Datalog_?

Datalog can express all _relational_ queries. _Recursion_ in Datalog is much easier to express,
much more powerful, and usually runs faster than in SQL. Datalog is also extremely composable:
you can build your queries piece by piece.

> Recursion is especially important for graph queries. CozoDB's dialect of Datalog
> supercharges it even further by allowing recursion through a safe subset of aggregations,
> and by providing extremely efficient canned algorithms (such as PageRank) for the kinds of recursions
> frequently required in graph analysis.
>
> As you learn Datalog, you will discover that the _rules_ of Datalog are like functions
> in a programming language. Rules are composable, and decomposing a query into rules
> can make it clearer and more maintainable, with no loss in efficiency.
> This is unlike the monolithic approach taken by the SQL `select-from-where` in nested forms,
> which can sometimes read like [golfing](https://en.wikipedia.org/wiki/Code_golf).

### Time travel?

Time travel in the database setting means
tracking changes to data over time
and allowing queries to be logically executed at a point in time
to get a historical view of the data.

> In a sense, this makes your database _immutable_,
> since nothing is really deleted from the database ever.
>
> In Cozo, instead of having all data automatically support
> time travel, we let you decide if you want the capability
> for each of your relation. Every extra functionality comes
> with its cost, and you don't want to pay the price if you don't use it.
>
> For the reason why you might want time travel for your data,
> we have written a [short story](https://docs.cozodb.org/en/latest/releases/v0.4.html).

### How performant?

On a 2020 Mac Mini with the RocksDB persistent storage engine (CozoDB supports many storage engines):

* Running OLTP queries for a relation with 1.6M rows, you can expect around 100K QPS (queries per second) for mixed
  read/write/update transactional queries, and more than 250K QPS for read-only queries, with database peak memory usage
  around 50MB.
* Speed for backup is around 1M rows per second, for restore is around 400K rows per second, and is insensitive to
  relation (table) size.
* For OLAP queries, it takes around 1 second (within a factor of 2, depending on the exact operations) to scan a table
  with 1.6M rows. The time a query takes scales roughly with the number of rows the query touches, with memory usage
  determined mainly by the size of the return set.
* Two-hop graph traversal completes in less than 1ms for a graph with 1.6M vertices and 31M edges.
* The Pagerank algorithm completes in around 50ms for a graph with 10K vertices and 120K edges, around 1 second for a
  graph with 100K vertices and 1.7M edges, and around 30 seconds for a graph with 1.6M vertices and 32M edges.

For more numbers and further details, we have a writeup
about performance [here](https://docs.cozodb.org/en/latest/releases/v0.3.html).

## Getting started

Usually, to learn a database, you need to install it first.
This is unnecessary for CozoDB as a testimony to its extreme embeddability, since you can run
a complete CozoDB instance in your browser, at near-native speed for most operations!

So open up the [CozoDB in WASM page](https://www.cozodb.org/wasm-demo/), and then:

* Follow the [tutorial](https://docs.cozodb.org/en/latest/tutorial.html).

Or you can skip ahead for the information about installing CozoDB into your favourite environment first.

### Teasers

If you are in a hurry and just want a taste of what querying with CozoDB is like, here it is.
In the following `*route` is a relation with two columns `fr` and `to`,
representing a route between those airports,
and `FRA` is the code for Frankfurt Airport.

How many airports are directly connected to `FRA`?

```
?[count_unique(to)] := *route{fr: 'FRA', to}
```

| count_unique(to) |
|------------------|
| 310              |

How many airports are reachable from `FRA` by one stop?

```
?[count_unique(to)] := *route{fr: 'FRA', to: stop},
                       *route{fr: stop, to}
```

| count_unique(to) |
|------------------|
| 2222             |

How many airports are reachable from `FRA` by any number of stops?

```
reachable[to] := *route{fr: 'FRA', to}
reachable[to] := reachable[stop], *route{fr: stop, to}
?[count_unique(to)] := reachable[to]
```

| count_unique(to) |
|------------------|
| 3462             |

What are the two most difficult-to-reach airports
by the minimum number of hops required,
starting from `FRA`?

```
shortest_paths[to, shortest(path)] := *route{fr: 'FRA', to},
                                      path = ['FRA', to]
shortest_paths[to, shortest(path)] := shortest_paths[stop, prev_path],
                                      *route{fr: stop, to},
                                      path = append(prev_path, to)
?[to, path, p_len] := shortest_paths[to, path], p_len = length(path)

:order -p_len
:limit 2
```

| to  | path                                                | p_len |
|-----|-----------------------------------------------------|-------|
| YPO | `["FRA","YYZ","YTS","YMO","YFA","ZKE","YAT","YPO"]` | 8     |
| BVI | `["FRA","AUH","BNE","ISA","BQL","BEU","BVI"]`       | 7     |

What is the shortest path between `FRA` and `YPO`, by actual distance travelled?

```
start[] <- [['FRA']]
end[] <- [['YPO]]
?[src, dst, distance, path] <~ ShortestPathDijkstra(*route[], start[], end[])
```

| src | dst | distance | path                                                      |
|-----|-----|----------|-----------------------------------------------------------|
| FRA | YPO | 4544.0   | `["FRA","YUL","YVO","YKQ","YMO","YFA","ZKE","YAT","YPO"]` |

CozoDB attempts to provide nice error messages when you make mistakes:

```
?[x, Y] := x = 1, y = x + 1
```

<pre><span style="color: rgb(204, 0, 0);">eval::unbound_symb_in_head</span><span>

  </span><span style="color: rgb(204, 0, 0);">×</span><span> Symbol 'Y' in rule head is unbound
   ╭────
 </span><span style="color: rgba(0, 0, 0, 0.5);">1</span><span> │ ?[x, Y] := x = 1, y = x + 1
   · </span><span style="font-weight: bold; color: rgb(255, 0, 255);">     ─</span><span>
   ╰────
</span><span style="color: rgb(0, 153, 255);">  help: </span><span>Note that symbols occurring only in negated positions are not considered bound
</span></pre>

## Install

We suggest that you [try out](#Getting-started) CozoDB before you install it in your environment.

How you install CozoDB depends on which environment you want to use it in.
Follow the links in the table below:

| Language/Environment                                     | Official platform support                                                                                               | Storage |
|----------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------|---------|
| [Python](https://github.com/cozodb/pycozo)               | Linux (x86_64), Mac (ARM64, x86_64), Windows (x86_64)                                                                   | MQR     |
| [NodeJS](./cozo-lib-nodejs)                              | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQR     |
| [Web browser](./cozo-lib-wasm)                           | Modern browsers supporting [web assembly](https://developer.mozilla.org/en-US/docs/WebAssembly#browser_compatibility)   | M       |
| [Java (JVM)](https://github.com/cozodb/cozo-lib-java)    | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQR     |
| [Clojure (JVM)](https://github.com/cozodb/cozo-clj)      | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQR     |
| [Android](https://github.com/cozodb/cozo-lib-android)    | Android (ARM64, ARMv7, x86_64, x86)                                                                                     | MQ      |
| [iOS/MacOS (Swift)](./cozo-lib-swift)                    | iOS (ARM64, simulators), Mac (ARM64, x86_64)                                                                            | MQ      |
| [Rust](https://docs.rs/cozo/)                            | Source only, usable on any [platform](https://doc.rust-lang.org/nightly/rustc/platform-support.html) with `std` support | MQRST   |
| [Golang](https://github.com/cozodb/cozo-lib-go)          | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQR     |
| [C/C++/language with C FFI](./cozo-lib-c)                | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQR     |
| [Standalone HTTP server](./cozo-bin)                     | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                                            | MQRST   |
| [Lisp](https://github.com/pegesund/cozodb-lisp)          | Linux (x86_64 so far)                                                                                                   | MR      |
| [Smalltalk](https://github.com/Mr-Dispatch/pharo-cozodb) | Win10 & Linux (Ubuntu 23.04) x86_64 tested, MacOS should probably work                                                  | MQR     |


For the storage column:

* M: in-memory, non-persistent backend
* Q: [SQLite](https://www.sqlite.org/) storage backend
* R: [RocksDB](http://rocksdb.org/) storage backend
* S: [Sled](https://github.com/spacejam/sled) storage backend
* T: [TiKV](https://tikv.org/) distributed storage backend

The [Rust doc](https://docs.rs/cozo/) has some tips on choosing storage,
which is helpful even if you are not using Rust.
Even if a storage/platform is not officially supported,
you can still try to compile your version to use, maybe with some tweaks in the code.

### Tuning the RocksDB backend for CozoDB

RocksDB has a lot of options, and by tuning them you can achieve better performance
for your workload. This is probably unnecessary for 95% of users, but if you are the
remaining 5%, CozoDB gives you the options to tune RocksDB directly if you are using the
RocksDB storage engine.

When you create the CozoDB instance with the RocksDB backend option, you are asked to
provide a path to a directory to store the data (will be created if it does not exist).
If you put a file named `options` inside this directory, the engine will expect this
to be a [RocksDB options file](https://github.com/facebook/rocksdb/wiki/RocksDB-Options-File)
and use it. If you are using the standalone `cozo` executable, you will get a log message if
this feature is activated.

Note that improperly set options can make your database misbehave!
In general, you should run your database once, copy the options file from `data/OPTIONS-XXXXXX`
from within your database directory, and use that as a base for your customization.
If you are not an expert on RocksDB, we suggest you limit your changes to adjusting those numerical
options that you at least have a vague understanding.

## Architecture

CozoDB consists of three layers stuck on top of each other,
with each layer only calling into the layer below:

<table>
<tbody>
<tr><td>(<i>User code</i>)</td></tr>
<tr><td>Language/environment wrapper</td></tr>
<tr><td>Query engine</td></tr>
<tr><td>Storage engine</td></tr>
<tr><td>(<i>Operating system</i>)</td></tr>
</tbody>
</table>

### Storage engine

The storage engine defines a storage `trait` for the storage backend, which is an interface
with required operations, mainly the provision of a key-value store for binary data
with range scan capabilities. There are various implementations:

* In-memory, non-persistent backend
* [SQLite](https://www.sqlite.org/) storage backend
* [RocksDB](http://rocksdb.org/) storage backend
* [Sled](https://github.com/spacejam/sled) storage backend
* [TiKV](https://tikv.org/) distributed storage backend

Depending on the build configuration, not all backends may be available
in a binary release.
The SQLite backend is special in that it is also used as the backup file format,
which allows the exchange of data between databases with different backends.
If you are using the database embedded in Rust, you can even provide your own
custom backend.

The storage engine also defines a _row-oriented_ binary data format, which the storage
engine implementation does not need to know anything about.
This format contains an implementation of the
[memcomparable format](https://github.com/facebook/mysql-5.6/wiki/MyRocks-record-format#memcomparable-format)
used for the keys, which enables the storage of rows of data as binary blobs
that, when sorted lexicographically, give the correct order.
This also means that data files for the SQLite backend cannot be queried with SQL
in the usual way, and access must be through the decoding process in CozoDB.

### Query engine

The query engine part provides various functionalities:

* function/aggregation/algorithm definitions
* database schema
* transaction
* query compilation
* query execution

This part is where most of
the code of CozoDB is concerned. The CozoScript manual [has a chapter](https://docs.cozodb.org/en/latest/execution.html)
about the execution process.

Users interact with the query engine with the [Rust API](https://docs.rs/cozo/).

### Language/environment wrapper

For all languages/environments except Rust, this part just translates the Rust API
into something that can be easily consumed by the targets. For Rust, there is no wrapper.
For example, in the case of the standalone server, the Rust API is translated
into HTTP endpoints, whereas in the case of NodeJS, the (synchronous) Rust API
is translated into a series of asynchronous calls from the JavaScript runtime.

If you want to make CozoDB usable in other languages, this part is where your focus
should be. Any existing generic interop libraries between Rust and your target language
would make the job much easier. Otherwise, you can consider wrapping the C API,
as this is supported by most languages. For the languages officially supported,
only Golang wraps the C API directly.

## Status of the project

CozoDB is still very young, but we encourage you to try it out for your use case.
Any feedback is welcome.

Versions before 1.0 do not promise syntax/API stability or storage compatibility.

## Links

* [Project page](https://cozodb.org/)
* [Documentation](https://docs.cozodb.org/en/latest/)
* [Main repo](https://github.com/cozodb/cozo)
* [Rust doc](https://docs.rs/cozo/)
* [Issue tracker](https://github.com/cozodb/cozo/issues)
* [Project discussions](https://github.com/cozodb/cozo/discussions)
* [User reddit](https://www.reddit.com/r/cozodb/)

## Licensing and contributing

This project is licensed under MPL-2.0 or later.
See [here](CONTRIBUTING.md) if you are interested in contributing to the project.
