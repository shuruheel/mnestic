# Transactional FTS corpus statistics

Ranked search computes document count and average document length from the same
posting population as the search: the storage snapshot plus the transaction's
own writes. Statistics count documents carrying postings, including composite
keys. Empty text and rows loaded without search-index maintenance contribute no
documents until their postings are built.

Each transaction keeps exact local statistics. The database also retains a
cache of committed statistics keyed by index relation ID. Transactions capture
a cache generation before opening their storage snapshot. Shared entries are
reused only while that generation remains current and no foreign FTS writer is
active. Older or overlapping transactions scan their own postings and retain
the result locally. They cannot repopulate the current shared cache.

Before a posting mutation, its local entry is invalidated and an active-writer
fence is acquired. Successful posting operations install exact local totals.
A successful, non-overlapping storage commit promotes those totals into the
shared cache. Abort, failed commit and unwind retain the fence until storage
teardown, then invalidate affected shared entries. Concurrent writers use a
conservative invalidation path so an old absolute total cannot overwrite their
combined committed corpus.

An interrupted posting operation marks its index uncertain. Legacy transaction
queries may inspect their pending posting view, but commit rejects an unresolved
incomplete FTS mutation. Subsequent mutation attempts use scans rather than
single-posting delta probes. A complete rebuild can restore the coherent-index
invariant. The usual response to a posting storage error is to abort.

Create, reindex, drop and raw restore participate in the same cache lifecycle.
Index identity prevents stale entries from surviving same-name recreation.
The cache remains in memory and requires no persisted schema change or shared
counter key. Existing ranking formulas and tokenization remain unchanged.

For serialized warm workloads, statistics access and successful incremental
maintenance remain constant-time. The first read after invalidation, cold open
or an overlapping snapshot may scan the index. Tests count those scans directly
through `FTS_DOC_STATS_SCANS` and verify scores after abort, storage errors,
rebuilds and concurrent RocksDB transactions. Cache coherence assumes one
database runtime over a storage namespace, as do the other runtime-owned caches.
