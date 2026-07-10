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

[ English | [中文](./README-zh.md) ]

## What mnestic adds over CozoDB

Upstream's last commit was 2024-12-04. mnestic continues the engine, with these
capabilities on top of it:

- **Cached graph projections** — `::graph create G { edges: knows }` names an
  in-memory adjacency that twelve graph algorithms reuse across queries instead
  of rebuilding on every call. Always fresh: a projection never serves data
  differing from what the consuming transaction's own scan would return, and a
  write to a source frees what was built from it.
  ([spec](docs/specs/graph-projection.md))
- **Bitemporality** — a `TxTime` column type with a crash-safe monotone commit
  clock, `:as_of` reads, the two-level `(valid time, transaction time)`
  resolution, and `::history` / `::history_gc` / `::evict`.
  ([spec](docs/specs/bitemporality.md))
- **Provenance semirings** — user-defined absorptive combines inside recursion
  (`Db::register_custom_aggr`), the `min_cost_k` bounded-meet aggregate returning
  the *k* best derivations with their evidence chains, and `:reconcile`
  recompute-based belief revision.
  ([spec](docs/specs/provenance-semirings.md))
- **In-engine hybrid retrieval** — reciprocal-rank fusion over vector, full-text
  and graph legs as one Datalog-composable fixed rule, with MMR diversification.
- **Read-only Cypher** — an openCypher subset translated to CozoScript (alpha;
  feature `cypher`, off by default).
  ([spec](docs/specs/cypher-read.md))
- **Faster lookups and plans** — equality pushdown turns post-filter point
  lookups into keyed seeks (~28× at 5k rows), plus a deterministic greedy join
  reorder and an opt-in factorized `count()` rewrite.
- **Non-blocking vector index builds** — HNSW builds in RAM in parallel and no
  longer blocks reads for minutes; search-path neighbour vectors batch-fetch
  through RocksDB `MultiGet`.
- **Operational recovery** — `::repair_corrupt` surgically deletes truncated
  tuples instead of forcing you to drop a database that fails an integrity check.
- **Interruptibility that works** — `::kill` and `:timeout` abort running
  queries, including long graph-adjacency builds.

Everything else — CozoScript, the storage engines, the data model — is upstream
CozoDB, unchanged unless noted in
[`CHANGELOG-FORK.md`](CHANGELOG-FORK.md).

## New in 0.11.0

**Cached graph projections.** Twelve graph algorithms can now take their
adjacency from a named, always-fresh, in-memory projection instead of rescanning
the edge relation and rebuilding a CSR on every call:

```
::graph create g { edges: knows, nodes: person }

?[node, group] <~ ConnectedComponents(graph: 'g')
?[node, rank]  <~ PageRank(graph: 'g', iterations: 20)
```

Measured on a 400,000-edge graph (*cold* is the positional form, i.e. the
previous behaviour):

| kernel | cold | warm | |
|---|---|---|---|
| `ConnectedComponents` | 127 ms | 7.9 ms | **16×** |
| `PageRank`, 20 iterations | 150 ms | 10 ms | **15×** |
| `ClusteringCoefficients` | 169 ms | 56 ms | 3× |

What is cached is the setup — scanning the edges and building the CSR — so the
gain shrinks as the kernel itself dominates. Under write churn the cache degrades
to build-per-query; it never goes stale. Projections are in-memory and are not
persisted.

Also in this release:

- **BREAKING (results):** `PageRank`'s default `iterations` is now 20, up from
  10, which was a below-upstream default and measurably non-convergent. Pass
  `iterations: 10` to restore the old numbers.
- **Fixed:** an empty edge relation used to abort the process in seven graph
  algorithms; and `multi_transaction` could deadlock a process by parking a
  `rayon` worker for the transaction's lifetime.
- `PageRank` accepts an optional node relation, so vertices with no edges are
  ranked instead of silently dropped.

Full detail, including the upgrade notes and known limitations, is in
[`CHANGELOG-FORK.md`](CHANGELOG-FORK.md).


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

> **mnestic** currently ships the Rust crate ([crates.io/mnestic](https://crates.io/crates/mnestic))
> and a Python binding ([PyPI `mnestic`](https://pypi.org/project/mnestic/) — `pip install mnestic`).
> The matrix below is upstream CozoDB's binding set, preserved for reference.

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

**Experimental (alpha):** a **read-only Cypher query surface** — translate an
openCypher subset to CozoScript so you can evaluate the engine without first
learning Datalog — is available behind the off-by-default `cypher` feature
(`DbInstance::run_cypher` / `cypher_to_script`; Python `run_cypher`). Design,
scope, and limitations: [`docs/specs/cypher-read.md`](docs/specs/cypher-read.md).
Datalog remains the native, full-power query language.

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
