# Validated scalar count scans

Status: implementation candidate, not published or performance-qualified.

## Motivation and bounded scope

MindGraph D10 observed two exact embedding-count native intervals of 8.052544 s
and 2.357726 s before a terminal 30-second API timeout. This is one exposed
instrumented development request, not a component comparison or proof that
allocation is the cause. The count influences ANN candidate breadth and must
remain exact. The API uses the ordinary `count` query without changes.

A bare `StoreTx::range_count` is insufficient: it skips value decoding and its
loop has no query poison. Neither stale cached counts nor estimates are allowed.
This candidate instead specializes the input iterator of one scalar builtin
`count` over a plain stored scan. No grouping, filter, actual join, expression,
derived relation or time-resolved scan qualifies. Row-preserving reorder and
unit-join wrappers are allowed. Other shapes retain the original evaluator.
The ordinary aggregation still counts every body match, including equal/null
arguments, and owns empty results, limits, output and memory accounting.

## Storage and decoder contract

`StoreTx::range_scan_for_count` has the same bounds and transaction as tuple
scanning and yields one success per validated row. Its default implementation
fully decodes rows. RocksDB uses the existing pinned iterator and borrowed
key/value pair. It still decodes the key. A canonical value blob holding exactly
one vector can be validated without allocation: exact MessagePack container and
enum shape, F32/F64 tag, binary length, element-width divisibility and exact end
of payload. VectorVisitor accepts arbitrary floating-point bits; no new numeric
restriction is imposed. Any unrecognized/noncanonical/malformed input falls
back to `try_decode_tuple_from_kv`, retaining its errors. This is validation of
an unused payload, not ignoring corruption or estimating a count.

No disk schema, bridge ABI, cache, counter or mutation path changes. The count
uses the transaction's existing snapshot and sees its own writes where supported.
Poison is checked before enumeration and after each iterator pull; native storage
calls remain as interruptible as the existing iterator. This is not a new claim
about cancelling an in-progress RocksDB call or a timed-out host worker.

## Required checks and experiment boundary

Tests compare count with ordinary full enumeration for empty/populated relations,
mutation, equal values, filters, joins, distinct aggregates, limits, missing
relations, deadlines, rollback, concurrent RocksDB snapshot readers and reopen.
Codec tests exercise F32/F64 and all binary-length classes, exhaustive one-byte
mutations and truncations of a canonical fixture, comparing success and error
text/code against the existing decoder. RocksDB corruption must remain a query
error. Hosted default and RocksDB jobs run these checks; no local native build.

A result does not yet establish speed or API non-regression. Before performance
execution freeze an immutable candidate and separate development protocol with
matched resources, immutable stored vectors, complete outputs, independent review
and fixed attempt/deadline rules. Component observations must precede a separate
complete API confirmation. Earlier D3/D9/D10 outcomes stay terminal; missing
private-positive coverage and all remaining release gates stay disclosed.
