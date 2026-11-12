# Cypher to CozoScript

A translation table for developers and coding agents who know Cypher. Every
CozoScript snippet below was executed against mnestic 0.19.0. mnestic also has
a read-only openCypher subset (`Db::run_cypher`, Rust, feature `cypher`, off by
default); this guide covers CozoScript, the language every binding exposes.

## The model in four differences

1. **Relations, not labels.** A node type is a stored relation with a key:
   `:create person {name: String => age: Int}`. Columns before `=>` form the
   primary key; a write with an existing key replaces that row.
2. **Edges are relations too.** `(a)-[:KNOWS]->(b)` is a row in
   `knows {src, dst}`. Traversal is a join on shared variable names.
3. **Results are sets.** Duplicate result rows collapse. `?[city] := *person{city}`
   returns each city once.
4. **Bind keys inside the braces.** `*person{name: $name, age}` seeks the primary
   key. `*person{name, age}, name == $name` scans the relation, then filters.
   The planner has no cost model, so the written form decides the plan.

## Reads

| Cypher | CozoScript |
|---|---|
| `MATCH (p:Person {name: $name}) RETURN p.age` | `?[age] := *person{name: $name, age}` |
| `MATCH (a {name: $n})-[:KNOWS]->(f) RETURN f` | `?[f] := *knows{src: $n, dst: f}` |
| `MATCH (a {name: $n})-[:KNOWS]-(f) RETURN f` (either direction) | two rules with the same head:<br>`?[f] := *knows{src: $n, dst: f}`<br>`?[f] := *knows{src: f, dst: $n}` |
| `WHERE p.city = 'nyc' AND p.age > 26` | `?[name, age] := *person{name, age, city}, city == 'nyc', age > 26` |
| `RETURN p.city, count(*), avg(p.age), collect(p.name)` | `?[city, count(name), mean(age), collect(name)] := *person{name, age, city}` |
| `ORDER BY p.age DESC SKIP 1 LIMIT 2` | `?[name, age] := *person{name, age} :order -age :limit 2 :offset 1` |
| `WHERE p.email IS NULL` | `?[n] := *person{name: n, email}, is_null(email)` (the column must be declared nullable: `email: String?`) |

Aggregates go in the rule **head** (`?[city, count(name)]`), never in the body.
A body term like `c = count(x)` fails with `eval::no_implementation`.

### Variable-length paths: `-[:KNOWS*1..3]->`

Write the hop count as a column with an explicit bound, so recursion terminates.
Then aggregate for the minimum:

```
reach[b, h] := *knows{src: $n, dst: b}, h = 1
reach[c, h] := reach[b, h0], *knows{src: b, dst: c}, h0 < 3, h = h0 + 1
?[who, min(h)] := reach[who, h]
```

### `OPTIONAL MATCH`

Union a matched rule with a negated rule that supplies the default:

```
deg[n, count(x)] := *knows{src: n, dst: x}
?[name, cnt] := *person{name}, deg[name, cnt]
?[name, cnt] := *person{name}, not deg[name, _], cnt = 0
```

Aggregate in a helper rule such as `deg`. Rules that share a head and form a
union cannot aggregate.

### Shortest path

Use a fixed rule over a projected edge relation:

```
e[s, d] := *knows{src: s, dst: d}
start[] <- [[$from]]
goal[] <- [[$to]]
?[from, to, cost, path] <~ ShortestPathDijkstra(e[], start[], goal[])
```

Project through a rule such as `e`. Passing the stored relation directly
(`*knows[src, dst]`) hands every column to the algorithm, and a third numeric
column is used as the edge weight.

## Writes

| Cypher | CozoScript |
|---|---|
| `MERGE (p:Person {name: $n}) SET p = {age: $a, ...}` (upsert, every column) | `?[name, age, email] <- [[$n, $a, $e]] :put person {name => age, email}` |
| `CREATE` that must fail if the key exists | `:insert person {name => age}` (fails with `transact::assertion_failure`) |
| `SET p.email = $e` (leave other columns unchanged) | `?[name, email] <- [[$n, $e]] :update person {name => email}` |
| `DELETE p` | `?[name] <- [[$n]] :rm person {name}` |
| `DETACH DELETE p` | one script, so all three commit or none do:<br>`{?[src, dst] := *knows{src, dst}, src == $n :rm knows {src, dst}}`<br>`{?[src, dst] := *knows{src, dst}, dst == $n :rm knows {src, dst}}`<br>`{?[name] <- [[$n]] :rm person {name}}` |
| `UNWIND $rows AS r CREATE ...` | `?[name, age] <- $rows :put person {name => age}` with `rows` as a list of lists |

**`:put` replaces the whole row.** With a partial column list, the omitted
columns are reset to their declared defaults, not kept: after
`:put person {name => age}`, a previously set `email` is back to `null`.
Cypher's `MERGE ... SET` keeps unmentioned properties. To change only some
columns, use `:update`.

In `DETACH DELETE`, the `dst == $n` filter scans `knows`, because `dst` is not
a key prefix. For frequent reverse lookups, add an index:
`::index create knows:by_dst {dst, src}`.

## Search

| Cypher (Neo4j 5 / Relate) | CozoScript |
|---|---|
| `SEARCH d IN (VECTOR INDEX doc_vec FOR $q LIMIT 2)` | `?[id, d] := ~doc:vec{id \| query: vec($q), k: 2, ef: 32, bind_distance: d} :order d` |
| `SEARCH d IN (FULLTEXT INDEX doc_fts FOR $q LIMIT 2)` | `?[id, s] := ~doc:fts{id \| query: $q, k: 2, bind_score: s} :order -s` |
| `COMBINE (VECTOR ..., FULLTEXT ...) USING WRRF` | `hybrid_search({...})` in Python and Rust: RRF over vector, full text and optional graph legs, with `detailed: True` for per-leg attribution |

Index creation:

```
::hnsw create doc:vec {dim: 384, m: 16, dtype: F32, fields: [emb], distance: Cosine, ef_construction: 64}
::fts create doc:fts {extractor: text, tokenizer: Simple, filters: [Lowercase]}
```

HNSW is approximate. For a selective filter (for example, top-k among one rare
type), score the eligible rows exactly instead of filtering an ANN result:

```
?[id, d] := *doc{id, kind: $kind, emb}, d = cos_dist(emb, vec($q)) :order d :limit 10
```

## Parameters and errors

Parameters are `$name`, as in Cypher, and are passed as a dict. Never build a
query by string formatting.

Errors arrive as an exception whose first argument is a diagnostic dict
(`code`, `message`, `causes`, `display`). Branch on `code`; show `display` to
humans. The [quickstart](quickstart.md) has a worked example.
