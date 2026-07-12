/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::default::Default;
use std::fmt::{Debug, Formatter};
use std::iter;
use std::path::Path;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[allow(unused_imports)]
use std::thread;
#[allow(unused_imports)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[allow(unused_imports)]
use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use crossbeam::sync::ShardedLock;
use either::{Left, Right};
use itertools::Itertools;
use miette::Report;
#[allow(unused_imports)]
use miette::{bail, ensure, miette, Diagnostic, IntoDiagnostic, Result, WrapErr};
use serde_json::json;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::functions::current_validity;
use crate::data::json::JsonValue;
use crate::data::program::{InputProgram, QueryAssertion, RelationOp, ReturnMutation};
use crate::data::relation::ColumnDef;
use crate::data::tuple::{Tuple, TupleT};
use crate::data::value::{DataValue, ValidityTs, LARGEST_UTF_CHAR};
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::fts::TokenizerCache;
use crate::parse::sys::{HnswIndexConfig, SysOp};
use crate::parse::{parse_expressions, parse_script, CozoScript, SourceSpan};
use crate::query::compile::{CompiledProgram, CompiledRule, CompiledRuleSet};
use crate::query::ra::{
    FilteredRA, FtsSearchRA, HnswSearchRA, InnerJoin, LshSearchRA, NegJoin, RelAlgebra, ReorderRA,
    StoredRA, StoredWithValidityRA, TempStoreRA, UnificationRA,
};
#[allow(unused_imports)]
use crate::runtime::callback::{
    CallbackCollector, CallbackDeclaration, CallbackOp, EventCallbackRegistry,
};
use crate::runtime::graph_projection;
use crate::runtime::graph_projection::ProjectionCache;
use crate::runtime::relation::{
    extend_tuple_from_v, AccessLevel, InsufficientAccessLevel, RelationHandle, RelationId,
};
use crate::runtime::transact::SessionTx;
use crate::runtime::tt_clock::{wall_clock_micros, TtClock};
use crate::storage::temp::TempStorage;
use crate::storage::{Storage, StoreTx};
use crate::{decode_tuple_from_kv, FixedRule, Symbol};

pub(crate) struct RunningQueryHandle {
    pub(crate) started_at: f64,
    pub(crate) poison: Poison,
}

pub(crate) struct RunningQueryCleanup {
    pub(crate) id: u64,
    pub(crate) running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
}

impl Drop for RunningQueryCleanup {
    fn drop(&mut self) {
        let mut map = self.running_queries.lock().unwrap();
        if let Some(handle) = map.remove(&self.id) {
            handle.poison.kill();
        }
    }
}

#[derive(serde_derive::Serialize, serde_derive::Deserialize)]
pub struct DbManifest {
    pub storage_version: u64,
}

/// Whether a script is mutable or immutable.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ScriptMutability {
    /// The script is mutable.
    Mutable,
    /// The script is immutable.
    Immutable,
}

/// Per-call options for [`Db::run_script_with_options`] (mnestic fork, query
/// budget). Deliberately future-proof: construct with [`ScriptRunOptions::new`]
/// / the `with_*` builders or `..Default::default()` so new options can be
/// added without breaking callers.
#[derive(Debug, Clone, Default)]
pub struct ScriptRunOptions {
    /// Per-call wall-clock budget in seconds. `None` (the default) imposes no
    /// per-call budget. The effective deadline for each statement is the
    /// minimum of this, any per-block `:timeout`, and the Db default
    /// ([`Db::set_default_query_timeout`]) — a `:timeout` can only tighten the
    /// budget, never extend past this guard. It is a single whole-script
    /// deadline: a multi-statement script does not get it afresh per statement.
    pub timeout: Option<f64>,
}

impl ScriptRunOptions {
    /// A fresh options set with no overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the per-call wall-clock budget in seconds (builder style).
    pub fn with_timeout(mut self, secs: f64) -> Self {
        self.timeout = Some(secs);
        self
    }
}

/// The database object of Cozo.
#[derive(Clone)]
pub struct Db<S> {
    pub(crate) db: S,
    temp_db: TempStorage,
    relation_store_id: Arc<AtomicU64>,
    /// Transaction-time commit clock high-water mark (mnestic fork,
    /// bitemporality step 2; `runtime/tt_clock.rs`). Seeded at open from
    /// `max(persisted TT_HWM system key, wall clock)`.
    tt_clock: Arc<TtClock>,
    /// Critical section for tt allocation + persist + commit (spec §13.10).
    tt_commit_lock: Arc<Mutex<()>>,
    /// User-registered aggregates (mnestic fork, provenance semirings R0b) —
    /// mirrors `fixed_rules`. In-memory, `Db`-scoped; consulted by the parser
    /// when a head aggregate is not a builtin. NOT consulted when parsing
    /// trigger scripts (custom aggregates in triggers are unsupported: a
    /// trigger is persisted CozoScript re-parsed on every write, while a
    /// factory is a process-scoped Rust closure that cannot persist — a
    /// fresh `Db` open would break every write to the relation. Permanent
    /// policy, recorded in R2: materialized OUTPUTS of custom aggregates
    /// persist fine; the operators themselves are registration-scoped).
    custom_aggrs: Arc<ShardedLock<BTreeMap<String, crate::data::aggr::RegisteredAggr>>>,
    custom_bounded_meets:
        Arc<ShardedLock<BTreeMap<String, crate::data::aggr::RegisteredBoundedMeet>>>,
    pub(crate) queries_count: Arc<AtomicU64>,
    pub(crate) running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
    pub(crate) fixed_rules: Arc<ShardedLock<BTreeMap<String, Arc<Box<dyn FixedRule>>>>>,
    pub(crate) tokenizers: Arc<TokenizerCache>,
    #[cfg(not(target_arch = "wasm32"))]
    callback_count: Arc<AtomicU32>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) event_callbacks: Arc<ShardedLock<EventCallbackRegistry>>,
    relation_locks: Arc<ShardedLock<BTreeMap<SmartString<LazyCompact>, Arc<ShardedLock<()>>>>>,
    /// Index relations currently being built off-lock (mnestic fork). The
    /// non-blocking HNSW build releases the relation write lock during the heavy
    /// build phase, so this set serialises concurrent builds of the *same* index
    /// (whose publication only becomes visible at the end) without re-blocking
    /// readers. Keyed by the index relation's full `base:idx` name.
    index_builds_in_progress: Arc<Mutex<BTreeSet<SmartString<LazyCompact>>>>,
    /// Cross-query cache of FTS corpus stats `(total_tokens, n_docs)` keyed by
    /// index relation name (mnestic fork, DEVELOPMENT.md Bet 1b avgdl). Only
    /// consulted for *legacy* FTS indexes that predate the durable doc-stats
    /// counter (no aggregate stored) and have not been written since — for those
    /// the corpus is immutable, so a single deduplicated scan per process is
    /// correct and needs no invalidation. Indexes built/migrated under the
    /// counter read it directly (O(1)) and never touch this cache.
    fts_doc_stats_cache: Arc<Mutex<BTreeMap<SmartString<LazyCompact>, (u64, u64)>>>,
    /// Db-wide default per-query wall-clock budget in milliseconds (mnestic
    /// fork, query budget). `0` = unset. Set via
    /// [`Db::set_default_query_timeout`]; folded (via `min`) into every
    /// script's effective deadline alongside any per-call timeout and per-block
    /// `:timeout`. Anchored at script start, so it bounds the whole
    /// `run_script` call rather than each statement.
    default_query_timeout_ms: Arc<AtomicU64>,
    /// Kill switch for the automatic factorized-count rewrite (mnestic fork,
    /// query factorization; `query/factorize.rs`). When `true`, an eligible
    /// single-clause `count()`-over-a-positive-join query is rewritten to
    /// Yannakakis-style counting sub-rules with a bit-identical answer. DEFAULT:
    /// `false` (opt-in) for this first release — a silently-wrong-count risk is
    /// not worth a default-on rewrite until it has soaked. To flip the default,
    /// change the `AtomicBool::new(false)` in [`Db::new`] to `true`.
    enable_factorize: Arc<AtomicBool>,
    /// Freshness substrate for cached graph projections (mnestic fork;
    /// `runtime/graph_projection.rs`, spec §3.3). Cloned into every
    /// `SessionTx`, which captures a watermark off it before its storage
    /// transaction pins and reports its dirty relations back at commit.
    pub(crate) graph_projections: Arc<ProjectionCache>,
    /// Test-only: make the next multi-transaction commit fail without attempting
    /// it (mnestic fork, 0.12.1). Commit failures are real but rare in
    /// production — an I/O error, a backend conflict — and there is no portable
    /// way to provoke one across `mem`/`sqlite`/`rocksdb`. The change-feed
    /// contract ("callbacks fire only for rows that committed") needs one, so
    /// this switch injects it. Set via [`Db::fail_next_commit_for_tests`].
    #[cfg(any(test, feature = "test-hooks"))]
    fail_next_commit: Arc<AtomicBool>,
}

impl<S> Debug for Db<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Db")
    }
}

/// RAII marker for an in-progress off-lock index build (mnestic fork): removes
/// the index name from the in-progress set on drop, covering every exit path
/// (success, error, or panic) so a failed build never wedges future ones.
struct IndexBuildGuard {
    set: Arc<Mutex<BTreeSet<SmartString<LazyCompact>>>>,
    name: SmartString<LazyCompact>,
}

impl Drop for IndexBuildGuard {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.name);
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Initialization of database failed")]
#[diagnostic(code(db::init))]
pub(crate) struct BadDbInit(#[help] pub(crate) String);

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot import data into relation {0} as it is an index")]
#[diagnostic(code(tx::import_into_index))]
pub(crate) struct ImportIntoIndex(pub(crate) String);

#[derive(serde_derive::Serialize, serde_derive::Deserialize, Debug, Clone, Default)]
/// Rows in a relation, together with headers for the fields.
pub struct NamedRows {
    /// The headers
    pub headers: Vec<String>,
    /// The rows
    pub rows: Vec<Tuple>,
    /// Contains the next named rows, if exists
    pub next: Option<Box<NamedRows>>,
}

impl IntoIterator for NamedRows {
    type Item = Tuple;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl NamedRows {
    /// create a named rows with the given headers and rows
    pub fn new(headers: Vec<String>, rows: Vec<Tuple>) -> Self {
        Self {
            headers,
            rows,
            next: None,
        }
    }

    /// If there are more named rows after the current one
    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// convert a chain of named rows to individual named rows
    pub fn flatten(self) -> Vec<Self> {
        let mut collected = vec![];
        let mut current = self;
        loop {
            let nxt = current.next.take();
            collected.push(current);
            if let Some(n) = nxt {
                current = *n;
            } else {
                break;
            }
        }
        collected
    }

    /// Convert to a JSON object
    pub fn into_json(self) -> JsonValue {
        let nxt = match self.next {
            None => json!(null),
            Some(more) => more.into_json(),
        };
        let rows = self
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(JsonValue::from).collect::<JsonValue>())
            .collect::<JsonValue>();
        json!({
            "headers": self.headers,
            "rows": rows,
            "next": nxt,
        })
    }
    /// Make named rows from JSON
    pub fn from_json(value: &JsonValue) -> Result<Self> {
        let headers = value
            .get("headers")
            .ok_or_else(|| miette!("NamedRows requires 'headers' field"))?;
        let headers = headers
            .as_array()
            .ok_or_else(|| miette!("'headers' field must be an array"))?;
        let headers = headers
            .iter()
            .map(|h| -> Result<String> {
                let h = h
                    .as_str()
                    .ok_or_else(|| miette!("'headers' field must be an array of strings"))?;
                Ok(h.to_string())
            })
            .try_collect()?;
        let rows = value
            .get("rows")
            .ok_or_else(|| miette!("NamedRows requires 'rows' field"))?;
        let rows = rows
            .as_array()
            .ok_or_else(|| miette!("'rows' field must be an array"))?;
        let rows = rows
            .iter()
            .map(|row| -> Result<Vec<DataValue>> {
                let row = row
                    .as_array()
                    .ok_or_else(|| miette!("'rows' field must be an array of arrays"))?;
                Ok(row.iter().map(DataValue::from).collect_vec())
            })
            .try_collect()?;
        Ok(Self {
            headers,
            rows,
            next: None,
        })
    }

    /// Create a query and parameters to apply an operation (insert, put, delete, rm) to a stored
    /// relation with the named rows.
    pub fn into_payload(self, relation: &str, op: &str) -> Payload {
        let cols_str = self.headers.join(", ");
        let query = format!("?[{cols_str}] <- $data :{op} {relation} {{ {cols_str} }}");
        let data = DataValue::List(self.rows.into_iter().map(DataValue::List).collect());
        (query, [("data".to_string(), data)].into())
    }
}

const STATUS_STR: &str = "status";
const OK_STR: &str = "OK";

/// The query and parameters.
pub type Payload = (String, BTreeMap<String, DataValue>);

/// Commands to be sent to a multi-transaction
#[derive(Eq, PartialEq, Debug)]
pub enum TransactionPayload {
    /// Commit the current transaction
    Commit,
    /// Abort the current transaction
    Abort,
    /// Run a query inside the transaction
    Query(Payload),
}

impl<'s, S: Storage<'s>> Db<S> {
    /// Create a new database object with the given storage.
    /// You must call [`initialize`](Self::initialize) immediately after creation.
    /// Due to lifetime restrictions we are not able to call that for you automatically.
    pub fn new(storage: S) -> Result<Self> {
        let ret = Self {
            db: storage,
            temp_db: Default::default(),
            relation_store_id: Default::default(),
            tt_clock: Default::default(),
            tt_commit_lock: Default::default(),
            custom_aggrs: Default::default(),
            custom_bounded_meets: Default::default(),
            queries_count: Default::default(),
            running_queries: Default::default(),
            fixed_rules: Arc::new(ShardedLock::new(DEFAULT_FIXED_RULES.clone())),
            tokenizers: Arc::new(Default::default()),
            #[cfg(not(target_arch = "wasm32"))]
            callback_count: Default::default(),
            // callback_receiver: Arc::new(receiver),
            #[cfg(not(target_arch = "wasm32"))]
            event_callbacks: Default::default(),
            relation_locks: Default::default(),
            index_builds_in_progress: Default::default(),
            fts_doc_stats_cache: Default::default(),
            default_query_timeout_ms: Default::default(),
            // DEFAULT for the automatic factorized-count rewrite. Flip this one
            // line to `AtomicBool::new(true)` to make the rewrite default-on.
            enable_factorize: Arc::new(AtomicBool::new(false)),
            graph_projections: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            fail_next_commit: Arc::new(AtomicBool::new(false)),
        };
        Ok(ret)
    }

    /// Must be called after creation of the database to initialize the runtime state.
    pub fn initialize(&'s self) -> Result<()> {
        self.load_last_ids()?;
        Ok(())
    }

    /// Run a multi-transaction. A command should be sent to `payloads`, and the result should be
    /// retrieved from `results`. A transaction ends when it receives a `Commit` or `Abort`,
    /// or when a query is not successful. After a transaction ends, sending / receiving from
    /// the channels will fail.
    ///
    /// Write transactions _may_ block other reads, but we guarantee that this does not happen
    /// for the RocksDB backend.
    pub fn run_multi_transaction(
        &'s self,
        is_write: bool,
        payloads: Receiver<TransactionPayload>,
        results: Sender<Result<NamedRows>>,
    ) {
        let tx = if is_write {
            self.transact_write()
        } else {
            self.transact()
        };
        let mut cleanups: Vec<(Vec<u8>, Vec<u8>)> = vec![];
        let mut tx = match tx {
            Ok(tx) => tx,
            Err(err) => {
                let _ = results.send(Err(err));
                return;
            }
        };

        let ts = current_validity();
        let callback_targets = self.current_callback_targets();
        let mut callback_collector = BTreeMap::new();
        let mut write_locks = BTreeMap::new();

        for payload in payloads {
            match payload {
                TransactionPayload::Commit => {
                    for (lower, upper) in cleanups {
                        if let Err(err) = tx.store_tx.del_range_from_persisted(&lower, &upper) {
                            eprintln!("{err:?}")
                        }
                    }

                    // mnestic fork (0.12.1): dispatch the change feed only when
                    // the commit actually succeeded. This used to call
                    // `send_callbacks` unconditionally after the commit, so a
                    // failed commit still published `Put`/`Rm` events for rows
                    // that were never durable — and anything syncing off the feed
                    // (a search mirror, an audit log, a cache) would silently
                    // diverge from the database. `register_callback`'s own
                    // contract says "when the requested relation are
                    // *successfully committed*", and the single-statement path
                    // (`execute_single`) has always got this right by committing
                    // with `?` before it dispatches. The `Abort` arm below is
                    // likewise correct — it breaks without dispatching.
                    let commit_result = self.commit_multi_tx(&mut tx);
                    let committed = commit_result.is_ok();
                    let _ = results.send(commit_result.map(|_| NamedRows::default()));
                    #[cfg(not(target_arch = "wasm32"))]
                    if committed && !callback_collector.is_empty() {
                        self.send_callbacks(callback_collector)
                    }

                    break;
                }
                TransactionPayload::Abort => {
                    let _ = results.send(Ok(NamedRows::default()));
                    break;
                }
                TransactionPayload::Query((script, params)) => {
                    let p = match parse_script(
                        &script,
                        &params,
                        &self.fixed_rules.read().unwrap(),
                        crate::data::aggr::CustomAggrRegistries {
                            meet: &self.custom_aggrs.read().unwrap(),
                            bounded: &self.custom_bounded_meets.read().unwrap(),
                        },
                        ts,
                    ) {
                        Ok(p) => p,
                        Err(err) => {
                            if results.send(Err(err)).is_err() {
                                break;
                            } else {
                                continue;
                            }
                        }
                    };

                    let p = match p.get_single_program() {
                        Ok(p) => p,
                        Err(err) => {
                            if results.send(Err(err)).is_err() {
                                break;
                            } else {
                                continue;
                            }
                        }
                    };
                    if let Some(write_lock_name) = p.needs_write_lock() {
                        match write_locks.entry(write_lock_name) {
                            Entry::Vacant(e) => {
                                let lock = self
                                    .obtain_relation_locks(iter::once(e.key()))
                                    .pop()
                                    .unwrap();
                                e.insert(lock);
                            }
                            Entry::Occupied(_) => {}
                        }
                    }

                    let res = self.execute_single_program(
                        p,
                        &mut tx,
                        &mut cleanups,
                        ts,
                        &callback_targets,
                        &mut callback_collector,
                    );
                    if results.send(res).is_err() {
                        break;
                    }
                }
            }
        }
    }

    /// This returns the set of fixed rule implementations for this specific backend.
    pub fn get_fixed_rules(&'s self) -> BTreeMap<String, Arc<Box<dyn FixedRule>>> {
        return self.fixed_rules.read().unwrap().clone();
    }

    /// Snapshot of the registered custom aggregates (mnestic fork, R0b).
    pub fn get_custom_aggrs(&'s self) -> BTreeMap<String, crate::data::aggr::RegisteredAggr> {
        self.custom_aggrs.read().unwrap().clone()
    }

    /// Snapshot of the registered dominance bounded-meet aggregates
    /// (mnestic fork, spec `docs/specs/antichain-bounded-meet.md`).
    pub fn get_custom_bounded_meets(
        &'s self,
    ) -> BTreeMap<String, crate::data::aggr::RegisteredBoundedMeet> {
        self.custom_bounded_meets.read().unwrap().clone()
    }

    /// Run the CozoScript passed in. The `params` argument is a map of parameters.
    pub fn run_script(
        &'s self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
    ) -> Result<NamedRows> {
        self.run_script_with_options(payload, params, mutability, ScriptRunOptions::default())
    }

    /// Run the CozoScript passed in with per-call [`ScriptRunOptions`] (mnestic
    /// fork, query budget). Currently the only option is a per-call wall-clock
    /// `timeout` (seconds). The effective budget for each statement is the
    /// minimum of that timeout, any per-block `:timeout` option, and the Db
    /// default ([`Db::set_default_query_timeout`]) — a `:timeout` can only
    /// tighten the budget, never extend past the per-call or Db-default guard.
    /// The per-call timeout is a single whole-script deadline (a multi-statement
    /// script does not get the budget afresh per statement); triggers fired by
    /// the script inherit it. A budget expiry aborts before any commit, so a
    /// killed mutable script leaves no partial writes.
    pub fn run_script_with_options(
        &'s self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
        options: ScriptRunOptions,
    ) -> Result<NamedRows> {
        let cur_vld = current_validity();
        self.run_script_ast_inner(
            parse_script(
                payload,
                &params,
                &self.get_fixed_rules(),
                crate::data::aggr::CustomAggrRegistries {
                    meet: &self.get_custom_aggrs(),
                    bounded: &self.get_custom_bounded_meets(),
                },
                cur_vld,
            )?,
            cur_vld,
            mutability,
            options.timeout,
        )
    }

    /// One-call hybrid retrieval (mnestic fork addition): HNSW + FTS (+ optional
    /// traversal) recall fused with Reciprocal Rank Fusion, optionally
    /// diversified with Maximal Marginal Relevance. Read-only. See
    /// [`crate::HybridSearch`].
    pub fn hybrid_search(&'s self, q: &crate::runtime::hybrid::HybridSearch) -> Result<NamedRows> {
        let (script, params) = crate::runtime::hybrid::build_hybrid_query(q)?;
        self.run_script(&script, params, ScriptMutability::Immutable)
    }

    /// Build the CozoScript that [`Db::hybrid_search`] would run, without
    /// executing it. See [`crate::HybridSearch`].
    pub fn hybrid_search_script(
        &'s self,
        q: &crate::runtime::hybrid::HybridSearch,
    ) -> Result<String> {
        Ok(crate::runtime::hybrid::build_hybrid_query(q)?.0)
    }

    /// Run the CozoScript passed in. The `params` argument is a map of parameters.
    pub fn run_script_read_only(
        &'s self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        self.run_script(payload, params, ScriptMutability::Immutable)
    }

    /// Run the AST CozoScript passed in.
    pub fn run_script_ast(
        &'s self,
        payload: CozoScript,
        cur_vld: ValidityTs,
        mutability: ScriptMutability,
    ) -> Result<NamedRows> {
        self.run_script_ast_inner(payload, cur_vld, mutability, None)
    }

    /// Dispatch a parsed script, computing the whole-script wall-clock deadline
    /// (mnestic fork, query budget) from the optional per-call timeout and the
    /// Db default, anchored at this call's start, and threading it onto the
    /// script's transaction. Sys ops (`::…`) carry no budget.
    fn run_script_ast_inner(
        &'s self,
        payload: CozoScript,
        cur_vld: ValidityTs,
        mutability: ScriptMutability,
        call_timeout: Option<f64>,
    ) -> Result<NamedRows> {
        let read_only = mutability == ScriptMutability::Immutable;
        let outer_deadline = self.effective_outer_deadline(call_timeout);
        match payload {
            CozoScript::Single(p) => self.execute_single(cur_vld, p, read_only, outer_deadline),
            CozoScript::Imperative(ps) => {
                self.execute_imperative(cur_vld, &ps, read_only, outer_deadline)
            }
            CozoScript::Sys(op) => self.run_sys_op(op, read_only),
        }
    }

    /// Compute the whole-script deadline from the per-call timeout and the Db
    /// default (mnestic fork, query budget), both anchored at one clock reading.
    /// Returns the earliest (`min`) of whichever are set; `None` if neither is
    /// set, if the budget overflows `Instant`, or if the target has no monotonic
    /// clock (wasm).
    fn effective_outer_deadline(&self, call_timeout: Option<f64>) -> Option<Instant> {
        let start = budget_now()?;
        let mut deadline = call_timeout.and_then(|secs| deadline_from_secs(start, secs));
        let default_ms = self.default_query_timeout_ms.load(Ordering::Relaxed);
        if default_ms > 0 {
            if let Some(d) = start.checked_add(Duration::from_millis(default_ms)) {
                deadline = Some(deadline.map_or(d, |existing| existing.min(d)));
            }
        }
        deadline
    }

    /// Commit a multi-transaction. Wraps `SessionTx::commit_tx` so the
    /// `test-hooks` failure switch has one place to live (mnestic fork, 0.12.1;
    /// see [`Db::fail_next_commit_for_tests`]). The injected failure returns
    /// before the storage commit is attempted, so the transaction rolls back —
    /// which is exactly the state a genuine commit failure leaves behind, and
    /// the state the change-feed contract is about.
    fn commit_multi_tx(&'s self, tx: &mut SessionTx<'_>) -> Result<()> {
        #[cfg(any(test, feature = "test-hooks"))]
        if self.fail_next_commit.swap(false, Ordering::SeqCst) {
            miette::bail!("injected commit failure (test-hooks)");
        }
        tx.commit_tx()
    }

    /// Test-only: make the next multi-transaction commit fail (mnestic fork,
    /// 0.12.1). One-shot — it clears itself when it fires.
    ///
    /// This is **not** a supported API: it is compiled only under the internal
    /// `test-hooks` feature. It exists because the change-feed contract
    /// ("subscribers receive events only for rows that actually committed")
    /// cannot otherwise be tested — a real commit failure needs an I/O error or
    /// a backend-specific conflict, and there is no portable way to provoke one
    /// across `mem` / `sqlite` / `rocksdb`.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn fail_next_commit_for_tests(&self) {
        self.fail_next_commit.store(true, Ordering::SeqCst);
    }

    /// Set a Db-wide default per-query wall-clock budget in seconds (mnestic
    /// fork, query budget). `None` (or a non-positive value) disables it. The
    /// default is folded (via `min`) into every subsequent script's effective
    /// deadline, so a per-block `:timeout` or a per-call timeout can only
    /// tighten it, never extend past it. Anchored at each `run_script` call's
    /// start, so it bounds the whole call rather than each statement.
    pub fn set_default_query_timeout(&self, secs: Option<f64>) {
        let ms = match secs {
            Some(s) if s > 0.0 => (s * 1000.0) as u64,
            _ => 0,
        };
        self.default_query_timeout_ms.store(ms, Ordering::Relaxed);
    }

    /// Enable or disable the automatic factorized-count rewrite (mnestic fork,
    /// query factorization; `query/factorize.rs`). The kill switch is a Db-wide
    /// toggle, default OFF for this release. When on, an eligible single-clause
    /// `count()`-over-a-positive-join is rewritten to Yannakakis-style counting
    /// sub-rules whose answer is bit-identical to the naive enumeration; every
    /// query the conservative trigger declines is evaluated exactly as before.
    pub fn set_query_factorization(&self, enabled: bool) {
        self.enable_factorize.store(enabled, Ordering::Relaxed);
    }

    /// Whether the automatic factorized-count rewrite is currently enabled
    /// (mnestic fork, query factorization).
    pub fn query_factorization(&self) -> bool {
        self.enable_factorize.load(Ordering::Relaxed)
    }

    /// The current Db-wide default per-query budget in seconds, or `None` if
    /// unset (mnestic fork, query budget).
    pub fn default_query_timeout(&self) -> Option<f64> {
        let ms = self.default_query_timeout_ms.load(Ordering::Relaxed);
        if ms == 0 {
            None
        } else {
            Some(ms as f64 / 1000.0)
        }
    }

    /// Export relations to JSON data.
    ///
    /// `relations` contains names of the stored relations to export.
    pub fn export_relations<I, T>(&'s self, relations: I) -> Result<BTreeMap<String, NamedRows>>
    where
        T: AsRef<str>,
        I: Iterator<Item = T>,
    {
        let tx = self.transact()?;
        let mut ret: BTreeMap<String, NamedRows> = BTreeMap::new();
        for rel in relations {
            let handle = tx.get_relation(rel.as_ref(), false)?;
            let size_hint = handle.metadata.keys.len() + handle.metadata.non_keys.len();

            if handle.access_level < AccessLevel::ReadOnly {
                bail!(InsufficientAccessLevel(
                    handle.name.to_string(),
                    "data export".to_string(),
                    handle.access_level
                ));
            }

            let mut cols = handle
                .metadata
                .keys
                .iter()
                .map(|col| col.name.clone())
                .collect_vec();
            cols.extend(
                handle
                    .metadata
                    .non_keys
                    .iter()
                    .map(|col| col.name.clone())
                    .collect_vec(),
            );

            let start = Tuple::default().encode_as_key(handle.id);
            let end = Tuple::default().encode_as_key(handle.id.next());

            let mut rows = vec![];
            for data in tx.store_tx.range_scan(&start, &end) {
                let (k, v) = data?;
                let tuple = decode_tuple_from_kv(&k, &v, Some(size_hint));
                rows.push(tuple);
            }
            let headers = cols.iter().map(|col| col.to_string()).collect_vec();
            ret.insert(rel.as_ref().to_string(), NamedRows::new(headers, rows));
        }
        Ok(ret)
    }
    /// Import relations. The argument `data` accepts data in the shape of
    /// what was returned by [Self::export_relations].
    /// The target stored relations must already exist in the database.
    /// Any associated B-tree indices will be updated; HNSW/FTS/LSH indices are
    /// **not** maintained — see the warning emitted per relation, and rebuild
    /// them with `::reindex`.
    ///
    /// Note that triggers and callbacks are _not_ run for the relations, if any exists.
    /// If you need to activate triggers or callbacks, use queries with parameters.
    pub fn import_relations(&'s self, data: BTreeMap<String, NamedRows>) -> Result<()> {
        #[derive(Debug, Diagnostic, Error)]
        #[error("cannot import data for relation '{0}': {1}")]
        #[diagnostic(code(import::bad_data))]
        struct BadDataForRelation(String, JsonValue);

        let rel_names = data.keys().map(SmartString::from).collect_vec();
        let locks = self.obtain_relation_locks(rel_names.iter());
        let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();

        let cur_vld = current_validity();

        let mut tx = self.transact_write()?;

        for (relation_op, in_data) in data {
            let is_delete;
            let relation: &str = match relation_op.strip_prefix('-') {
                None => {
                    is_delete = false;
                    &relation_op
                }
                Some(s) => {
                    is_delete = true;
                    s
                }
            };
            if relation.contains(':') {
                bail!(ImportIntoIndex(relation.to_string()))
            }
            let handle = tx.get_relation(relation, false)?;
            // Graph-projection dirty-set hook (spec §3.4 row 7). Marked at
            // relation resolution, so it covers all three import modes: plain
            // put, the `-`-prefixed delete, and the TxTime-buffered branch.
            tx.mark_dirty(&handle);
            let has_indices = !handle.indices.is_empty();

            // Bulk import maintains B-tree secondary indices (below) but NOT
            // the HNSW/FTS/LSH indices, whose maintenance runs off the per-row
            // :put path this bypasses — imported rows silently stay invisible
            // to vector/text/LSH search until the index is rebuilt. Warn
            // loudly rather than corrupt-silently; a hard error would break
            // legitimate import-then-reindex callers. (mnestic fork, hardening)
            warn_if_indexes_stranded(&handle, relation, "bulk import into");

            if handle.access_level < AccessLevel::Protected {
                bail!(InsufficientAccessLevel(
                    handle.name.to_string(),
                    "data import".to_string(),
                    handle.access_level
                ));
            }

            let header2idx: BTreeMap<_, _> = in_data
                .headers
                .iter()
                .enumerate()
                .map(|(i, k)| -> Result<(&str, usize)> { Ok((k as &str, i)) })
                .try_collect()?;

            // tt-stamped relations (mnestic fork, bitemporality step 3): the
            // import must not carry tt (engine-assigned), deletes need :rm,
            // and rows are buffered so the whole import — one transaction —
            // is stamped with ONE tt (one belief event, spec §13.3).
            if handle.has_txtime() {
                if is_delete {
                    bail!(
                        "cannot import deletes into TxTime relation '{}': use :rm",
                        relation
                    );
                }
                let tt_name = &handle.metadata.keys.last().unwrap().name;
                if header2idx.contains_key(tt_name as &str) {
                    bail!(
                        "column {} of relation '{}' is engine-assigned at commit and cannot be imported",
                        tt_name,
                        relation
                    );
                }
                let kpos = handle.metadata.keys.len() - 1;
                let mut rows = Vec::with_capacity(in_data.rows.len());
                for row in &in_data.rows {
                    let mut extracted = Vec::with_capacity(kpos + handle.metadata.non_keys.len());
                    for col in handle.metadata.keys[..kpos]
                        .iter()
                        .chain(handle.metadata.non_keys.iter())
                    {
                        let idx = header2idx.get(&col.name as &str).ok_or_else(|| {
                            miette!(
                                "required header {} not found for relation {}",
                                col.name,
                                relation
                            )
                        })?;
                        let v = row
                            .get(*idx)
                            .ok_or_else(|| miette!("row too short: {:?}", row))?;
                        extracted.push(col.typing.coerce(v.clone(), cur_vld)?);
                    }
                    rows.push(extracted);
                }
                if !rows.is_empty() {
                    tx.pending_tt_writes
                        .push(crate::runtime::transact::PendingTtWrite {
                            handle: handle.clone(),
                            rows,
                            is_retract: false,
                            span: Default::default(),
                        });
                }
                continue;
            }

            let key_indices: Vec<_> = handle
                .metadata
                .keys
                .iter()
                .map(|col| -> Result<(usize, &ColumnDef)> {
                    let idx = header2idx.get(&col.name as &str).ok_or_else(|| {
                        miette!(
                            "required header {} not found for relation {}",
                            col.name,
                            relation
                        )
                    })?;
                    Ok((*idx, col))
                })
                .try_collect()?;

            let val_indices: Vec<_> = if is_delete {
                vec![]
            } else {
                handle
                    .metadata
                    .non_keys
                    .iter()
                    .map(|col| -> Result<(usize, &ColumnDef)> {
                        let idx = header2idx.get(&col.name as &str).ok_or_else(|| {
                            miette!(
                                "required header {} not found for relation {}",
                                col.name,
                                relation
                            )
                        })?;
                        Ok((*idx, col))
                    })
                    .try_collect()?
            };

            for row in in_data.rows {
                let keys: Vec<_> = key_indices
                    .iter()
                    .map(|(i, col)| -> Result<DataValue> {
                        let v = row
                            .get(*i)
                            .ok_or_else(|| miette!("row too short: {:?}", row))?;
                        col.typing.coerce(v.clone(), cur_vld)
                    })
                    .try_collect()?;
                let k_store = handle.encode_key_for_store(&keys, Default::default())?;
                if has_indices {
                    if let Some(existing) = tx.store_tx.get(&k_store, false)? {
                        let mut old = keys.clone();
                        extend_tuple_from_v(&mut old, &existing);
                        if is_delete || old != row {
                            for (idx_rel, extractor) in handle.indices.values() {
                                let idx_tup =
                                    extractor.iter().map(|i| old[*i].clone()).collect_vec();
                                let encoded =
                                    idx_rel.encode_key_for_store(&idx_tup, Default::default())?;
                                tx.store_tx.del(&encoded)?;
                            }
                        }
                    }
                }
                if is_delete {
                    tx.store_tx.del(&k_store)?;
                } else {
                    let vals: Vec<_> = val_indices
                        .iter()
                        .map(|(i, col)| -> Result<DataValue> {
                            let v = row
                                .get(*i)
                                .ok_or_else(|| miette!("row too short: {:?}", row))?;
                            col.typing.coerce(v.clone(), cur_vld)
                        })
                        .try_collect()?;
                    let v_store = handle.encode_val_only_for_store(&vals, Default::default())?;
                    tx.store_tx.put(&k_store, &v_store)?;
                    if has_indices {
                        let mut kv = keys;
                        kv.extend(vals);
                        for (idx_rel, extractor) in handle.indices.values() {
                            let idx_tup = extractor.iter().map(|i| kv[*i].clone()).collect_vec();
                            let encoded =
                                idx_rel.encode_key_for_store(&idx_tup, Default::default())?;
                            tx.store_tx.put(&encoded, &[])?;
                        }
                    }
                }
            }
        }
        tx.commit_tx()?;
        Ok(())
    }
    /// Backup the running database into an Sqlite file
    #[allow(unused_variables)]
    pub fn backup_db(&'s self, out_file: impl AsRef<Path>) -> Result<()> {
        #[cfg(feature = "storage-sqlite")]
        {
            let sqlite_db = crate::new_cozo_sqlite(out_file)?;
            if sqlite_db.relation_store_id.load(Ordering::SeqCst) != 0 {
                bail!("Cannot create backup: data exists in the target database.");
            }
            let mut tx = self.transact()?;
            let iter = tx.store_tx.range_scan(&[], &[0xFF]);
            sqlite_db.db.batch_put(iter)?;
            tx.commit_tx()?;
            Ok(())
        }
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled")
    }
    /// Restore from an Sqlite backup
    #[allow(unused_variables)]
    pub fn restore_backup(&'s self, in_file: impl AsRef<Path>) -> Result<()> {
        // Graph-projection dirty-set hook (spec §3.4 row 13). Defensive: the
        // precondition below admits only an empty store, in which no relation
        // exists to project. But the restore writes through `batch_put`,
        // outside any `SessionTx`, so no dirty set could ever see it — and the
        // check runs after this point, on an error path a caller may ignore.
        self.graph_projections.invalidate_all();
        #[cfg(feature = "storage-sqlite")]
        {
            let sqlite_db = crate::new_cozo_sqlite(in_file)?;
            let mut s_tx = sqlite_db.transact()?;
            {
                let mut tx = self.transact()?;
                let store_id = tx.relation_store_id.load(Ordering::SeqCst);
                if store_id != 0 {
                    bail!(
                        "Cannot restore backup: data exists in the current database. \
                You can only restore into a new database (store id: {}).",
                        store_id
                    );
                }
                tx.commit_tx()?;
            }
            let iter = s_tx.store_tx.total_scan();
            self.db.batch_put(iter)?;
            s_tx.commit_tx()?;
            // Re-seed the tt commit clock from the restored high-water mark
            // (mnestic fork, bitemporality): the backup carries the TT_HWM
            // system key, and "persisted HWM >= every committed tt" holds
            // inside any consistent backup, so the persisted mark alone
            // suffices — no row scan. fetch_max seeding cannot regress.
            {
                let tx = self.transact()?;
                let persisted = tx.read_persisted_tt_hwm()?;
                self.tt_clock.seed(persisted);
            }
            Ok(())
        }
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled")
    }
    /// Import data from relations in a backup file.
    /// The target stored relations must already exist in the database, and it must not
    /// have any associated indices. If you want to import into relations with indices,
    /// use [Db::import_relations].
    ///
    /// Note that triggers and callbacks are _not_ run for the relations, if any exists.
    /// If you need to activate triggers or callbacks, use queries with parameters.
    #[allow(unused_variables)]
    pub fn import_from_backup(
        &'s self,
        in_file: impl AsRef<Path>,
        relations: &[String],
    ) -> Result<()> {
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled");

        #[cfg(feature = "storage-sqlite")]
        {
            let rel_names = relations.iter().map(SmartString::from).collect_vec();
            let locks = self.obtain_relation_locks(rel_names.iter());
            let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();

            let source_db = crate::new_cozo_sqlite(in_file)?;
            let mut src_tx = source_db.transact()?;
            let mut dst_tx = self.transact_write()?;

            for relation in relations {
                if relation.contains(':') {
                    bail!(ImportIntoIndex(relation.to_string()))
                }
                let src_handle = src_tx.get_relation(relation, false)?;
                let dst_handle = dst_tx.get_relation(relation, false)?;
                // Graph-projection dirty-set hook (spec §3.4 row 8): the rows
                // below go straight to `store_tx.put`, bypassing the mutation
                // paths that would otherwise mark this relation.
                dst_tx.mark_dirty(&dst_handle);

                if dst_handle.has_txtime() || src_handle.has_txtime() {
                    bail!(
                        "cannot import TxTime relation '{}' from a backup: its rows carry \
                         transaction times from the source store's clock, which this store's \
                         high-water mark knows nothing about — restore the full store with \
                         restore_backup() (which re-seeds the commit clock) or re-ingest",
                        relation
                    );
                }

                if !dst_handle.indices.is_empty() {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("Cannot import data into relation {0} from backup as the relation has indices")]
                    #[diagnostic(code(tx::bare_import_with_indices))]
                    #[diagnostic(help("Use `import_relations()` instead"))]
                    pub(crate) struct RestoreIntoRelWithIndices(pub(crate) String);

                    bail!(RestoreIntoRelWithIndices(dst_handle.name.to_string()))
                }

                // mnestic fork (0.12.1): the B-tree bail above is the only guard
                // this path had. It then raw-puts the source's KV rows straight
                // into the store, so a relation carrying HNSW/FTS/LSH indexes had
                // its rows restored underneath them with **zero signal** — the
                // operator restores a backup and hybrid retrieval silently returns
                // nothing for the restored rows. `import_relations` has warned
                // about exactly this since the fork's hardening pass; backup
                // restore never did. Same warning, one shared helper, so they
                // cannot drift apart again.
                warn_if_indexes_stranded(&dst_handle, relation, "backup restore into");

                if dst_handle.access_level < AccessLevel::Protected {
                    bail!(InsufficientAccessLevel(
                        dst_handle.name.to_string(),
                        "data import".to_string(),
                        dst_handle.access_level
                    ));
                }

                let src_lower = Tuple::default().encode_as_key(src_handle.id);
                let src_upper = Tuple::default().encode_as_key(src_handle.id.next());

                let data_it = src_tx.store_tx.range_scan(&src_lower, &src_upper).map(
                    |src_pair| -> Result<(Vec<u8>, Vec<u8>)> {
                        let (mut src_k, mut src_v) = src_pair?;
                        dst_handle.amend_key_prefix(&mut src_k);
                        dst_handle.amend_key_prefix(&mut src_v);
                        Ok((src_k, src_v))
                    },
                );
                for result in data_it {
                    let (key, val) = result?;
                    dst_tx.store_tx.put(&key, &val)?;
                }
            }

            src_tx.commit_tx()?;
            dst_tx.commit_tx()
        }
    }
    /// Register a custom fixed rule implementation.
    pub fn register_fixed_rule<R>(&self, name: String, rule_impl: R) -> Result<()>
    where
        R: FixedRule + 'static,
    {
        match self.fixed_rules.write().unwrap().entry(name) {
            Entry::Vacant(ent) => {
                ent.insert(Arc::new(Box::new(rule_impl)));
                Ok(())
            }
            Entry::Occupied(ent) => {
                bail!(
                    "A fixed rule with the name {} is already registered",
                    ent.key()
                )
            }
        }
    }

    /// Unregister a custom fixed rule implementation.
    pub fn unregister_fixed_rule(&self, name: &str) -> Result<bool> {
        if DEFAULT_FIXED_RULES.contains_key(name) {
            bail!("Cannot unregister builtin fixed rule {}", name);
        }
        Ok(self.fixed_rules.write().unwrap().remove(name).is_some())
    }

    /// Register a custom aggregate — a user-supplied ⊕ operator usable in
    /// rule heads by `name` (mnestic fork, provenance semirings R0b; see
    /// `RegisteredAggr` for the registrant's contract). With
    /// `is_meet = true` the aggregate is admitted into recursive rules — the
    /// ⊕ MUST then be an absorptive semilattice operation. Builtin names are
    /// reserved (the parser tries builtins first, so shadowing would be
    /// silent); duplicate registrations error — unregister first to replace
    /// (programs already parsed keep their factory `Arc`).
    pub fn register_custom_aggr<F>(&self, name: String, is_meet: bool, factory: F) -> Result<()>
    where
        F: Fn() -> Box<dyn crate::data::aggr::MeetAggrObj> + Send + Sync + 'static,
    {
        if crate::data::aggr::parse_aggr(&name).is_some() {
            bail!(
                "Cannot register custom aggregate {}: builtin names are reserved",
                name
            );
        }
        if crate::data::expr::get_op(&name).is_some() {
            // antichain-bounded-meet spec §3.1: builtin FUNCTION names are
            // reserved too — the namespaces are technically separate, which
            // would make the collision silent (one token, two semantics).
            bail!(
                "Cannot register custom aggregate {}: builtin function names are reserved",
                name
            );
        }
        let name_ok = !name.is_empty()
            && name.chars().next().unwrap().is_ascii_lowercase()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            bail!(
                "Cannot register custom aggregate {}: the name must be a lowercase identifier, or the grammar can never reach it",
                name
            );
        }
        // Fixed lock order (custom_aggrs, then custom_bounded_meets) in both
        // registration paths: closes the cross-registry TOCTOU without
        // deadlock risk.
        let mut aggrs = self.custom_aggrs.write().unwrap();
        if self
            .custom_bounded_meets
            .read()
            .unwrap()
            .contains_key(&name)
        {
            bail!(
                "A custom bounded-meet aggregate with the name {} is already registered",
                name
            );
        }
        match aggrs.entry(name) {
            Entry::Vacant(ent) => {
                ent.insert(crate::data::aggr::RegisteredAggr {
                    is_meet,
                    factory: Arc::new(factory),
                });
                Ok(())
            }
            Entry::Occupied(ent) => {
                bail!(
                    "A custom aggregate with the name {} is already registered",
                    ent.key()
                )
            }
        }
    }

    /// Unregister a custom aggregate. Programs already parsed keep working
    /// (they hold the factory `Arc`); future parses no longer resolve it.
    pub fn unregister_custom_aggr(&self, name: &str) -> Result<bool> {
        Ok(self.custom_aggrs.write().unwrap().remove(name).is_some())
    }

    /// Register a dominance bounded-meet aggregate (mnestic fork, spec
    /// `docs/specs/antichain-bounded-meet.md`): the head form `name(operand)`
    /// keeps, per group, the antichain (non-dominated / Pareto set) of
    /// operands under `dominates`, each survivor as its own output row.
    ///
    /// `dominates` MUST be a strict partial order (irreflexive + transitive)
    /// and pure, and must not panic; debug builds probe irreflexivity and
    /// asymmetry on encountered values. It receives ONLY the aggregated
    /// operand — pack every field it inspects. `max_survivors` is a
    /// mandatory resource guard: a group's non-dominated set exceeding it is
    /// a loud error, never a silent truncation. Builtin aggregate AND
    /// builtin function names are reserved; duplicates error (unregister
    /// first to replace; parsed programs keep their `Arc`). In-memory and
    /// `Db`-scoped: persisted output rows are plain data, readable after
    /// reopen with no re-registration, but re-running the query then needs
    /// the registration again. Nothing fingerprints the algebra — version
    /// names like schemas (e.g. `antichain_authority_v2`).
    pub fn register_bounded_meet_aggr<F>(
        &self,
        name: String,
        dominates: F,
        max_survivors: usize,
    ) -> Result<()>
    where
        F: Fn(&DataValue, &DataValue) -> bool + Send + Sync + 'static,
    {
        if crate::data::aggr::parse_aggr(&name).is_some() {
            bail!(
                "Cannot register bounded-meet aggregate {}: builtin names are reserved",
                name
            );
        }
        if crate::data::expr::get_op(&name).is_some() {
            bail!(
                "Cannot register bounded-meet aggregate {}: builtin function names are reserved",
                name
            );
        }
        let name_ok = !name.is_empty()
            && name.chars().next().unwrap().is_ascii_lowercase()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            bail!(
                "Cannot register bounded-meet aggregate {}: the name must be a lowercase identifier, or the grammar can never reach it",
                name
            );
        }
        if max_survivors < 1 {
            bail!(
                "Cannot register bounded-meet aggregate {}: max_survivors must be at least 1",
                name
            );
        }
        // Same fixed lock order as register_custom_aggr (custom_aggrs first):
        // the read guard is held across the insert to close the TOCTOU.
        let aggrs = self.custom_aggrs.read().unwrap();
        if aggrs.contains_key(&name) {
            bail!(
                "A custom aggregate with the name {} is already registered",
                name
            );
        }
        match self.custom_bounded_meets.write().unwrap().entry(name) {
            Entry::Vacant(ent) => {
                ent.insert(crate::data::aggr::RegisteredBoundedMeet {
                    dominates: Arc::new(dominates),
                    max_survivors,
                });
                Ok(())
            }
            Entry::Occupied(ent) => {
                bail!(
                    "A custom bounded-meet aggregate with the name {} is already registered",
                    ent.key()
                )
            }
        }
    }

    /// Unregister a dominance bounded-meet aggregate. Programs already
    /// parsed keep working (they hold the registration by value); future
    /// parses no longer resolve it.
    pub fn unregister_bounded_meet_aggr(&self, name: &str) -> Result<bool> {
        Ok(self
            .custom_bounded_meets
            .write()
            .unwrap()
            .remove(name)
            .is_some())
    }

    /// Register callback channel to receive changes when the requested relation are successfully committed.
    /// The returned ID can be used to unregister the callback channel.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn register_callback(
        &self,
        relation: &str,
        capacity: Option<usize>,
    ) -> (u32, Receiver<(CallbackOp, NamedRows, NamedRows)>) {
        let (sender, receiver) = if let Some(c) = capacity {
            bounded(c)
        } else {
            unbounded()
        };
        let cb = CallbackDeclaration {
            dependent: SmartString::from(relation),
            sender,
        };

        let mut guard = self.event_callbacks.write().unwrap();
        let new_id = self.callback_count.fetch_add(1, Ordering::SeqCst);
        guard
            .1
            .entry(SmartString::from(relation))
            .or_default()
            .insert(new_id);

        guard.0.insert(new_id, cb);
        (new_id, receiver)
    }

    /// Unregister callbacks/channels to run when changes to relations are committed.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn unregister_callback(&self, id: u32) -> bool {
        let mut guard = self.event_callbacks.write().unwrap();
        let ret = guard.0.remove(&id);
        if let Some(cb) = &ret {
            guard.1.get_mut(&cb.dependent).unwrap().remove(&id);

            if guard.1.get(&cb.dependent).unwrap().is_empty() {
                guard.1.remove(&cb.dependent);
            }
        }
        ret.is_some()
    }

    pub(crate) fn obtain_relation_locks<'a, T: Iterator<Item = &'a SmartString<LazyCompact>>>(
        &'s self,
        rels: T,
    ) -> Vec<Arc<ShardedLock<()>>> {
        let mut collected = vec![];
        let mut pending = vec![];
        {
            let locks = self.relation_locks.read().unwrap();
            for rel in rels {
                match locks.get(rel) {
                    None => {
                        pending.push(rel);
                    }
                    Some(lock) => collected.push(lock.clone()),
                }
            }
        }
        if !pending.is_empty() {
            let mut locks = self.relation_locks.write().unwrap();
            for rel in pending {
                let lock = locks.entry(rel.clone()).or_default().clone();
                collected.push(lock);
            }
        }
        collected
    }

    fn compact_relation(&'s self) -> Result<()> {
        let l = Tuple::default().encode_as_key(RelationId(0));
        let u = vec![DataValue::Bot].encode_as_key(RelationId(u64::MAX));
        self.db.range_compact(&l, &u)?;
        Ok(())
    }

    fn load_last_ids(&'s self) -> Result<()> {
        let mut tx = self.transact_write()?;
        self.relation_store_id
            .store(tx.init_storage()?.0, Ordering::Release);
        // Seed the tt commit clock (mnestic fork, bitemporality step 2):
        // max(persisted high-water mark, wall clock).
        self.tt_clock.seed(tx.read_persisted_tt_hwm()?);
        tx.commit_tx()?;
        Ok(())
    }

    /// The transaction-time commit clock (mnestic fork; test/introspection
    /// access — production allocation goes through `commit_tx_with_tt`).
    #[allow(dead_code)]
    pub(crate) fn tt_clock(&self) -> &TtClock {
        &self.tt_clock
    }
    pub(crate) fn transact(&'s self) -> Result<SessionTx<'s>> {
        // Read the watermark BEFORE opening the storage transaction: opening
        // is pinning, and the protocol's correctness rests on the capture
        // being sequenced before the snapshot (graph_projection.rs §3.3). A
        // struct-literal field would be evaluated in source order, which is
        // too easy to reorder by accident — hence the separate binding.
        let watermark = self.graph_projections.watermark();
        let ret = SessionTx {
            store_tx: Box::new(self.db.transact(false)?),
            temp_store_tx: self.temp_db.transact(true)?,
            relation_store_id: self.relation_store_id.clone(),
            temp_store_id: Default::default(),
            tokenizers: self.tokenizers.clone(),
            fts_doc_stats_cache: self.fts_doc_stats_cache.clone(),
            tt_clock: self.tt_clock.clone(),
            tt_commit_lock: self.tt_commit_lock.clone(),
            pending_tt_writes: Vec::new(),
            tt_hwm_dirty: false,
            reconciled_tt_relations: Default::default(),
            script_deadline: None,
            projections: self.graph_projections.clone(),
            watermark,
            dirty_relations: Default::default(),
            commit_inflight: false,
        };
        Ok(ret)
    }
    pub(crate) fn transact_write(&'s self) -> Result<SessionTx<'s>> {
        // See `transact`: the watermark capture must precede the pin.
        let watermark = self.graph_projections.watermark();
        let ret = SessionTx {
            store_tx: Box::new(self.db.transact(true)?),
            temp_store_tx: self.temp_db.transact(true)?,
            relation_store_id: self.relation_store_id.clone(),
            temp_store_id: Default::default(),
            tokenizers: self.tokenizers.clone(),
            fts_doc_stats_cache: self.fts_doc_stats_cache.clone(),
            tt_clock: self.tt_clock.clone(),
            tt_commit_lock: self.tt_commit_lock.clone(),
            pending_tt_writes: Vec::new(),
            tt_hwm_dirty: false,
            reconciled_tt_relations: Default::default(),
            script_deadline: None,
            projections: self.graph_projections.clone(),
            watermark,
            dirty_relations: Default::default(),
            commit_inflight: false,
        };
        Ok(ret)
    }

    pub(crate) fn execute_single_program(
        &'s self,
        p: InputProgram,
        tx: &mut SessionTx<'_>,
        cleanups: &mut Vec<(Vec<u8>, Vec<u8>)>,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
    ) -> Result<NamedRows> {
        #[allow(unused_variables)]
        let sleep_opt = p.out_opts.sleep;
        let (q_res, q_cleanups) =
            self.run_query(tx, p, cur_vld, callback_targets, callback_collector, true)?;
        cleanups.extend(q_cleanups);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(secs) = sleep_opt {
            thread::sleep(Duration::from_micros((secs * 1000000.) as u64));
        }
        Ok(q_res)
    }

    fn execute_single(
        &'s self,
        cur_vld: ValidityTs,
        p: InputProgram,
        read_only: bool,
        outer_deadline: Option<Instant>,
    ) -> Result<NamedRows, Report> {
        let mut callback_collector = BTreeMap::new();
        let write_lock_names = p.needs_write_lock();
        let is_write = write_lock_names.is_some();
        if read_only && is_write {
            bail!("write lock required for read-only query");
        }
        let write_lock = self.obtain_relation_locks(write_lock_names.iter());
        let _write_lock_guards = if is_write {
            Some(write_lock[0].read().unwrap())
        } else {
            None
        };
        let callback_targets = if is_write {
            self.current_callback_targets()
        } else {
            Default::default()
        };
        let mut cleanups = vec![];
        let res;
        {
            let mut tx = if is_write {
                self.transact_write()?
            } else {
                self.transact()?
            };
            // Carry the whole-script wall-clock budget on the tx so run_query
            // (and any triggers it fires) honour it (mnestic fork, query budget).
            tx.script_deadline = outer_deadline;

            res = self.execute_single_program(
                p,
                &mut tx,
                &mut cleanups,
                cur_vld,
                &callback_targets,
                &mut callback_collector,
            )?;

            for (lower, upper) in cleanups {
                tx.store_tx.del_range_from_persisted(&lower, &upper)?;
            }

            tx.commit_tx()?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if !callback_collector.is_empty() {
            self.send_callbacks(callback_collector)
        }

        Ok(res)
    }
    fn explain_compiled(
        &self,
        strata: &[CompiledProgram],
        advisory: Option<String>,
    ) -> Result<NamedRows> {
        let mut ret: Vec<JsonValue> = vec![];
        const STRATUM: &str = "stratum";
        const ATOM_IDX: &str = "atom_idx";
        const OP: &str = "op";
        const RULE_IDX: &str = "rule_idx";
        const RULE_NAME: &str = "rule";
        const REF_NAME: &str = "ref";
        const OUT_BINDINGS: &str = "out_relation";
        const JOINS_ON: &str = "joins_on";
        const FILTERS: &str = "filters/expr";

        let headers = vec![
            STRATUM.to_string(),
            RULE_IDX.to_string(),
            RULE_NAME.to_string(),
            ATOM_IDX.to_string(),
            OP.to_string(),
            REF_NAME.to_string(),
            JOINS_ON.to_string(),
            FILTERS.to_string(),
            OUT_BINDINGS.to_string(),
        ];

        for (stratum, p) in strata.iter().enumerate() {
            let mut clause_idx = -1;
            for (rule_name, v) in p {
                match v {
                    CompiledRuleSet::Rules(rules) => {
                        for CompiledRule { aggr, relation, .. } in rules.iter() {
                            clause_idx += 1;
                            let mut ret_for_relation = vec![];
                            let mut rel_stack = vec![relation];
                            let mut idx = 0;
                            let mut atom_type = "out";
                            for (a, _) in aggr.iter().flatten() {
                                if a.is_meet {
                                    if atom_type == "out" {
                                        atom_type = "meet_aggr_out";
                                    }
                                } else if a.is_bounded_meet {
                                    if atom_type == "out" {
                                        atom_type = "bounded_meet_aggr_out";
                                    }
                                } else {
                                    atom_type = "aggr_out";
                                }
                            }

                            ret_for_relation.push(json!({
                                STRATUM: stratum,
                                ATOM_IDX: idx,
                                OP: atom_type,
                                RULE_IDX: clause_idx,
                                RULE_NAME: rule_name.to_string(),
                                OUT_BINDINGS: relation.bindings_after_eliminate().into_iter().map(|v| v.to_string()).collect_vec()
                            }));
                            idx += 1;

                            while let Some(rel) = rel_stack.pop() {
                                // mnestic (join-reorder, 0.10.5): flag a join that
                                // shares no bound variables with its left input as a
                                // Cartesian step, so an N-way blow-up is visible in
                                // the plan (the greedy reorder also `log::warn`s).
                                let mut is_cartesian = false;
                                let (atom_type, ref_name, joins_on, filters) = match rel {
                                    r @ RelAlgebra::Fixed(..) => {
                                        if r.is_unit() {
                                            continue;
                                        }
                                        ("fixed", json!(null), json!(null), json!(null))
                                    }
                                    RelAlgebra::TempStore(TempStoreRA {
                                        storage_key,
                                        filters,
                                        ..
                                    }) => (
                                        "load_mem",
                                        json!(storage_key.to_string()),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::Stored(StoredRA {
                                        storage, filters, ..
                                    }) => (
                                        "load_stored",
                                        json!(format!(":{}", storage.name)),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::StoredWithValidity(StoredWithValidityRA {
                                        storage,
                                        filters,
                                        ..
                                    }) => (
                                        "load_stored_with_validity",
                                        json!(format!(":{}", storage.name)),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::StoredBitemporal(
                                        crate::query::ra::StoredBitemporalRA {
                                            storage,
                                            filters,
                                            ..
                                        },
                                    ) => (
                                        "load_stored_bitemporal",
                                        json!(format!(":{}", storage.name)),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::Join(inner) => {
                                        if inner.left.is_unit() {
                                            rel_stack.push(&inner.right);
                                            continue;
                                        }
                                        let t = inner.join_type();
                                        let InnerJoin {
                                            left,
                                            right,
                                            joiner,
                                            ..
                                        } = inner.as_ref();
                                        is_cartesian = joiner.left_keys.is_empty();
                                        rel_stack.push(left);
                                        rel_stack.push(right);
                                        (t, json!(null), json!(joiner.as_map()), json!(null))
                                    }
                                    RelAlgebra::NegJoin(inner) => {
                                        let t = inner.join_type();
                                        let NegJoin {
                                            left,
                                            right,
                                            joiner,
                                            ..
                                        } = inner.as_ref();
                                        rel_stack.push(left);
                                        rel_stack.push(right);
                                        (t, json!(null), json!(joiner.as_map()), json!(null))
                                    }
                                    RelAlgebra::Reorder(ReorderRA { relation, .. }) => {
                                        rel_stack.push(relation);
                                        ("reorder", json!(null), json!(null), json!(null))
                                    }
                                    RelAlgebra::Filter(FilteredRA {
                                        parent,
                                        filters: pred,
                                        ..
                                    }) => {
                                        rel_stack.push(parent);
                                        (
                                            "filter",
                                            json!(null),
                                            json!(null),
                                            json!(pred.iter().map(|f| f.to_string()).collect_vec()),
                                        )
                                    }
                                    RelAlgebra::Unification(UnificationRA {
                                        parent,
                                        binding,
                                        expr,
                                        is_multi,
                                        ..
                                    }) => {
                                        rel_stack.push(parent);
                                        (
                                            if *is_multi { "multi-unify" } else { "unify" },
                                            json!(binding.name),
                                            json!(null),
                                            json!(expr.to_string()),
                                        )
                                    }
                                    RelAlgebra::HnswSearch(HnswSearchRA {
                                        hnsw_search, ..
                                    }) => (
                                        "hnsw_index",
                                        json!(format!(":{}", hnsw_search.query.name)),
                                        json!(hnsw_search.query.name),
                                        json!(hnsw_search
                                            .filter
                                            .iter()
                                            .map(|f| f.to_string())
                                            .collect_vec()),
                                    ),
                                    RelAlgebra::FtsSearch(FtsSearchRA { fts_search, .. }) => (
                                        "fts_index",
                                        json!(format!(":{}", fts_search.query.name)),
                                        json!(fts_search.query.name),
                                        json!(fts_search
                                            .filter
                                            .iter()
                                            .map(|f| f.to_string())
                                            .collect_vec()),
                                    ),
                                    RelAlgebra::LshSearch(LshSearchRA { lsh_search, .. }) => (
                                        "lsh_index",
                                        json!(format!(":{}", lsh_search.query.name)),
                                        json!(lsh_search.query.name),
                                        json!(lsh_search
                                            .filter
                                            .iter()
                                            .map(|f| f.to_string())
                                            .collect_vec()),
                                    ),
                                };
                                let op_str = if is_cartesian {
                                    format!("{atom_type} (cartesian)")
                                } else {
                                    atom_type.to_string()
                                };
                                ret_for_relation.push(json!({
                                    STRATUM: stratum,
                                    ATOM_IDX: idx,
                                    OP: op_str,
                                    RULE_IDX: clause_idx,
                                    RULE_NAME: rule_name.to_string(),
                                    REF_NAME: ref_name,
                                    OUT_BINDINGS: rel.bindings_after_eliminate().into_iter().map(|v| v.to_string()).collect_vec(),
                                    JOINS_ON: joins_on,
                                    FILTERS: filters,
                                }));
                                idx += 1;
                            }
                            ret_for_relation.reverse();
                            ret.extend(ret_for_relation)
                        }
                    }
                    CompiledRuleSet::Fixed(_) => ret.push(json!({
                        STRATUM: stratum,
                        ATOM_IDX: 0,
                        OP: "algo",
                        RULE_IDX: 0,
                        RULE_NAME: rule_name.to_string(),
                    })),
                }
            }
        }

        // mnestic fork (query factorization): the detector advisory rides along
        // as a final row (op = `factorize_advisory`, message in the filters
        // column) so it is visible in `::explain` output as well as the log.
        if let Some(msg) = advisory {
            ret.push(json!({
                OP: "factorize_advisory",
                FILTERS: msg,
            }));
        }

        let rows = ret
            .into_iter()
            .map(|m| {
                headers
                    .iter()
                    .map(|i| DataValue::from(m.get(i).unwrap_or(&JsonValue::Null)))
                    .collect_vec()
            })
            .collect_vec();

        Ok(NamedRows::new(headers, rows))
    }
    /// Build an HNSW index into the in-RAM temp store, then publish its data to
    /// the live store (mnestic fork). On engines that support SST ingest the
    /// finished, key-sorted graph is bulk-loaded via `ingest_sorted`, which
    /// bypasses the transaction write-batch overlay entirely; other engines fall
    /// back to the per-key temp->store flush.
    ///
    /// Ordering invariant: the index *data* is ingested here, while the index
    /// *metadata* `put` (written transactionally by `create_hnsw_index`) only
    /// becomes visible at the enclosing `commit_tx`. So the keys an index
    /// references always exist on disk before any reader can observe the index —
    /// never the reverse.
    fn build_and_publish_hnsw(
        &'s self,
        tx: &mut SessionTx<'_>,
        config: &HnswIndexConfig,
    ) -> Result<()> {
        let idx_id = tx.create_hnsw_index(config)?;
        if self.db.supports_sst_ingest() {
            let lower = Tuple::default().encode_as_key(idx_id);
            let upper = Tuple::default().encode_as_key(idx_id.next());
            self.db
                .ingest_sorted(tx.temp_store_tx.range_scan(&lower, &upper))?;
        } else {
            tx.flush_temp_index_to_store(idx_id)?;
        }
        Ok(())
    }

    /// Build an HNSW index WITHOUT holding the per-relation write lock during the
    /// (multi-minute) graph construction, so concurrent reads are never blocked
    /// (mnestic fork). The write lock is taken only briefly to set up the empty
    /// index relation (Phase A) and to reconcile + publish (Phase D); the heavy
    /// build and the SST bulk-load happen lock-free in between.
    ///
    /// Consistency: the build reads every vector from one read-transaction
    /// snapshot, so the bulk graph is self-consistent. Base-relation rows that
    /// change during the unlocked window are folded in by `reconcile_hnsw_index`
    /// under the Phase-D lock. Ordering: the index data is ingested (live) before
    /// its metadata is committed, so a reader can never observe the index before
    /// its keys exist. RocksDB only (requires SST ingest).
    fn create_hnsw_index_nonblocking(&'s self, config: &HnswIndexConfig) -> Result<()> {
        let idx_full_name = SmartString::<LazyCompact>::from(format!(
            "{}:{}",
            config.base_relation, config.index_name
        ));
        // Serialise concurrent builds of the *same* index: publication is
        // deferred past the unlocked window, so `has_index` alone can't catch a
        // racing builder. The RAII guard clears the marker on every exit path.
        let _build_guard = {
            let mut set = self.index_builds_in_progress.lock().unwrap();
            if !set.insert(idx_full_name.clone()) {
                bail!("an index build for `{idx_full_name}` is already in progress");
            }
            IndexBuildGuard {
                set: self.index_builds_in_progress.clone(),
                name: idx_full_name,
            }
        };

        let rel_lock = self
            .obtain_relation_locks(iter::once(&config.base_relation))
            .pop()
            .unwrap();

        // PHASE A — brief write lock: create the empty index relation and commit,
        // so its relation id is durably consumed (never reused, even on crash).
        let (idx_handle, manifest, filter) = {
            let _guard = rel_lock.write().unwrap();
            let mut tx = self.transact_write()?;
            let (_rel_handle, idx_handle, manifest, filter) = tx.prepare_hnsw_index(config)?;
            tx.commit_tx()?;
            (idx_handle, manifest, filter)
        };
        let idx_id = idx_handle.id;
        let lower = Tuple::default().encode_as_key(idx_id);
        let upper = Tuple::default().encode_as_key(idx_id.next());
        let filter_ref = if filter.is_empty() {
            None
        } else {
            Some(&filter)
        };

        // PHASE B/C — NO lock: scan + build the graph in the in-RAM temp store,
        // then bulk-publish it into the live store via SST ingest. Every vector
        // read uses this read transaction's single snapshot, so the graph is
        // self-consistent; mutations after this snapshot are caught in Phase D.
        let snapshot_tuples: Vec<Tuple> = {
            let mut tx = self.transact()?;
            let base_handle = tx.get_relation(&config.base_relation, false)?;
            let snapshot_tuples: Vec<Tuple> =
                base_handle.scan_all(&tx).collect::<Result<Vec<_>>>()?;
            let mut idx_temp = idx_handle.clone();
            idx_temp.is_temp = true;
            // The build only *writes* the temp store (is_temp routing) and only
            // *reads* the base relation via this snapshot — so a read transaction
            // is sufficient and holds no relation lock.
            tx.hnsw_build_index(
                &manifest,
                &base_handle,
                &idx_temp,
                filter_ref,
                &snapshot_tuples,
            )?;
            self.db
                .ingest_sorted(tx.temp_store_tx.range_scan(&lower, &upper))?;
            snapshot_tuples
        };

        // PHASE D — brief write lock: reconcile mutations from the unlocked
        // window, then publish the metadata. Data is already live (ingested);
        // metadata becomes visible at commit, strictly after the data.
        {
            let _guard = rel_lock.write().unwrap();
            let mut tx = self.transact_write()?;
            let rel_handle = tx.get_relation(&config.base_relation, true)?;
            if rel_handle.has_index(&config.index_name) {
                // Lost a race (or the relation was rebuilt): our ingested data is
                // orphaned at idx_id — drop it and report the conflict.
                tx.store_tx.del_range_from_persisted(&lower, &upper)?;
                tx.commit_tx()?;
                bail!(
                    "index `{}` already exists on relation `{}`",
                    config.index_name,
                    config.base_relation
                );
            }
            tx.reconcile_hnsw_index(
                &manifest,
                &rel_handle,
                &idx_handle,
                filter_ref,
                &snapshot_tuples,
            )?;
            tx.insert_hnsw_index_meta(rel_handle, idx_handle, manifest, &config.index_name)?;
            tx.commit_tx()?;
        }
        Ok(())
    }

    /// The reserved eviction-audit relation (mnestic fork, bitemporality
    /// step 5), created lazily on first `::evict`. Keys: (relation, key
    /// marker, tt); value: rows_deleted.
    fn tt_evict_audit_relation(&'s self, tx: &mut SessionTx<'_>) -> Result<RelationHandle> {
        use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
        let col = |name: &str, coltype: ColType| ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
            default_gen: None,
        };
        let meta = StoredRelationMetadata {
            keys: vec![
                col("relation", ColType::String),
                col("key", ColType::String),
                col("tt", ColType::Int),
            ],
            non_keys: vec![col("rows_deleted", ColType::Int)],
        };
        if let Ok(h) = tx.get_relation("mnestic_evict_audit", false) {
            // The audit puts are raw store writes: a divergent schema would
            // corrupt rows, and indices/triggers would silently not be
            // maintained.
            if h.metadata != meta
                || !h.has_no_index()
                || !h.put_triggers.is_empty()
                || !h.rm_triggers.is_empty()
                || !h.replace_triggers.is_empty()
            {
                bail!(
                    "the relation name mnestic_evict_audit is reserved for the eviction \
                     audit trail; the existing relation must keep the exact reserved \
                     schema and carry no indices or triggers"
                );
            }
            return Ok(h);
        }
        tx.create_relation(crate::runtime::relation::InputRelationHandle {
            name: Symbol::new("mnestic_evict_audit", Default::default()),
            metadata: meta,
            key_bindings: vec![],
            dep_bindings: vec![],
            span: Default::default(),
        })
    }

    pub(crate) fn run_sys_op_with_tx(
        &'s self,
        tx: &mut SessionTx<'_>,
        op: &SysOp,
        read_only: bool,
        skip_locking: bool,
    ) -> Result<NamedRows> {
        match op {
            SysOp::Explain(prog) => {
                let (normalized_program, _) = prog.clone().into_normalized_program(tx)?;
                // mnestic fork (query factorization): show the rewritten plan
                // when the kill switch is on, and surface the detector advisory
                // as an extra `::explain` row either way.
                let (normalized_program, advisory) =
                    crate::query::factorize::maybe_rewrite_and_advise(
                        normalized_program,
                        self.enable_factorize.load(Ordering::Relaxed),
                    );
                let (stratified_program, _) = normalized_program.into_stratified_program()?;
                let program = stratified_program.magic_sets_rewrite(tx)?;
                let compiled = tx.stratified_magic_compile(program)?;
                self.explain_compiled(&compiled, advisory)
            }
            SysOp::Compact => {
                if read_only {
                    bail!("Cannot compact in read-only mode");
                }
                self.compact_relation()?;
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::ListRelations => self.list_relations(tx),
            SysOp::ListFixedRules => {
                let rules = self.fixed_rules.read().unwrap();
                Ok(NamedRows::new(
                    vec!["rule".to_string()],
                    rules
                        .keys()
                        .map(|k| vec![DataValue::from(k as &str)])
                        .collect_vec(),
                ))
            }
            // mnestic fork, graph projection (`docs/specs/graph-projection.md`
            // §3.1). The registry is process-local in-memory state, so these
            // arms take no relation locks and are trivially correct under
            // `skip_locking` — but `::graph create` reads the catalog through
            // `tx` to validate its sources.
            SysOp::CreateGraph { name, edges, nodes } => {
                if read_only {
                    bail!("Cannot create a graph projection in read-only mode");
                }
                graph_projection::sysop_create_graph(tx, name, edges, nodes.as_deref())?;
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::DropGraph(name) => {
                if read_only {
                    bail!("Cannot drop a graph projection in read-only mode");
                }
                graph_projection::sysop_drop_graph(tx, name)?;
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::ListGraphs => graph_projection::sysop_list_graphs(tx),
            SysOp::TtHistory(rel, keys, limit, offset) => {
                // mnestic fork, bitemporality step 5: the introspection
                // surface — every (vt, tt) record of the given keys.
                let handle = tx.get_relation(rel, false)?;
                if !handle.has_txtime() {
                    bail!("::history requires a TxTime relation, {} is not", rel);
                }
                if handle.access_level < AccessLevel::ReadOnly {
                    bail!(InsufficientAccessLevel(
                        handle.name.to_string(),
                        "history introspection".to_string(),
                        handle.access_level
                    ))
                }
                let kpos = handle.metadata.keys.len() - 1;
                let is_bitemporal = !handle.is_tt_only();
                let plain_len = if is_bitemporal { kpos - 1 } else { kpos };

                let mut headers: Vec<String> = handle.metadata.keys[..plain_len]
                    .iter()
                    .map(|c| c.name.to_string())
                    .collect();
                if is_bitemporal {
                    headers.push("vt_ts".to_string());
                }
                headers.push("op".to_string());
                headers.push("tt".to_string());
                headers.extend(handle.metadata.non_keys.iter().map(|c| c.name.to_string()));
                {
                    // consumers key rows by header name — a user column named
                    // `op`/`tt`/`vt_ts` would silently shadow one
                    let mut seen = std::collections::BTreeSet::new();
                    for h in &headers {
                        if !seen.insert(h.as_str()) {
                            bail!(
                                "::history synthesizes an output column named {}, which \
                                 collides with a column of {}; rename the column",
                                h,
                                rel
                            );
                        }
                    }
                }

                // spec §7: output is key-ascending
                let mut keys = keys.clone();
                keys.sort();
                keys.dedup();

                let mut rows: Vec<Vec<DataValue>> = Vec::new();
                for key in &keys {
                    if key.len() != plain_len {
                        bail!(
                            "::history key arity mismatch for {}: expected {} plain key column(s), got {}",
                            rel,
                            plain_len,
                            key.len()
                        );
                    }
                    let key = key
                        .iter()
                        .zip(handle.metadata.keys.iter())
                        .map(|(v, c)| {
                            c.typing
                                .coerce(v.clone(), crate::data::functions::MAX_VALIDITY_TS)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    for found in handle.scan_prefix(tx, &key) {
                        let t = found?;
                        let mut row: Vec<DataValue> = t[..plain_len].to_vec();
                        let (op, tt_us) = if is_bitemporal {
                            let (vt_ts, vt_flag) = match &t[kpos - 1] {
                                DataValue::Validity(v) => (v.timestamp.0 .0, v.is_assert.0),
                                _ => bail!("corrupt vt component in {}", rel),
                            };
                            row.push(DataValue::from(vt_ts));
                            let tt_us = match &t[kpos] {
                                DataValue::Validity(v) => v.timestamp.0 .0,
                                _ => bail!("corrupt tt component in {}", rel),
                            };
                            (if vt_flag { "assert" } else { "retract" }, tt_us)
                        } else {
                            match &t[kpos] {
                                DataValue::Validity(v) => (
                                    if v.is_assert.0 { "assert" } else { "retract" },
                                    v.timestamp.0 .0,
                                ),
                                _ => bail!("corrupt tt component in {}", rel),
                            }
                        };
                        row.push(DataValue::from(op));
                        row.push(DataValue::from(tt_us));
                        row.extend_from_slice(&t[kpos + 1..]);
                        rows.push(row);
                    }
                }
                // spec §7 ordering: key-asc, vt-desc, tt-desc. The raw scan
                // interleaves a vt-group's assert and retract RUNS (physical
                // order), which misreads as the belief timeline.
                rows.sort_by(|a, b| {
                    a[..plain_len].cmp(&b[..plain_len]).then_with(|| {
                        if is_bitemporal {
                            b[plain_len]
                                .cmp(&a[plain_len])
                                .then_with(|| b[plain_len + 2].cmp(&a[plain_len + 2]))
                        } else {
                            b[plain_len + 1].cmp(&a[plain_len + 1])
                        }
                    })
                });
                let offset = offset.unwrap_or(0);
                let rows: Vec<_> = rows
                    .into_iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect();
                Ok(NamedRows::new(headers, rows))
            }
            SysOp::TtHistoryGc(rel, cutoff) => {
                if read_only {
                    bail!("Cannot run ::history_gc in read-only mode");
                }
                let locks = if skip_locking {
                    vec![]
                } else {
                    self.obtain_relation_locks([&rel.name].into_iter())
                };
                let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();

                let mut handle = tx.get_relation(rel, true)?;
                // Graph-projection dirty-set hook (spec §3.4 row 11).
                tx.mark_dirty(&handle);
                if !handle.has_txtime() {
                    bail!("::history_gc requires a TxTime relation, {} is not", rel);
                }
                if handle.access_level < AccessLevel::Normal {
                    bail!(InsufficientAccessLevel(
                        handle.name.to_string(),
                        "history garbage collection".to_string(),
                        handle.access_level
                    ))
                }
                if tx
                    .pending_tt_writes
                    .iter()
                    .any(|w| w.handle.name == handle.name)
                {
                    bail!(
                        "relation {} has pending transaction-time writes in this transaction; \
                         commit them in their own transaction before running ::history_gc",
                        rel
                    );
                }
                let kpos = handle.metadata.keys.len() - 1;
                let is_bitemporal = !handle.is_tt_only();
                let cutoff = *cutoff;
                // A future cutoff can only be a typo — it would floor tts
                // that haven't been allocated yet.
                let present = wall_clock_micros().max(tx.tt_clock.peek());
                if cutoff > present {
                    bail!(
                        "::history_gc cutoff {} is in the future (present high-water mark: {})",
                        cutoff,
                        present
                    );
                }

                // Group per (plain key [, vt-ts]); within each group KEEP the
                // record the resolution would pick at tt = cutoff (greatest
                // tt <= cutoff, assert-wins on ties) plus everything at or
                // above the cutoff; delete the rest. NOTE v1 runs in one
                // transaction — chunked online gc is deferred until real
                // stores need it (recorded deviation).
                let mut group: Vec<(Vec<u8>, i64, bool)> = Vec::new(); // (key bytes, tt, is_assert-ish)
                let mut group_id: Option<Vec<DataValue>> = None;
                let mut to_delete: Vec<Vec<u8>> = Vec::new();
                let mut deleted = 0usize;
                let flush = |group: &mut Vec<(Vec<u8>, i64, bool)>,
                             to_delete: &mut Vec<Vec<u8>>| {
                    // keeper = greatest tt <= cutoff; tie -> assert
                    let keeper = group
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, tt, _))| *tt <= cutoff)
                        .max_by(|(_, (_, ta, aa)), (_, (_, tb, ab))| {
                            ta.cmp(tb).then_with(|| aa.cmp(ab))
                        })
                        .map(|(i, _)| i);
                    for (i, (kb, tt, _)) in group.iter().enumerate() {
                        if *tt < cutoff && Some(i) != keeper {
                            to_delete.push(kb.clone());
                        }
                    }
                    group.clear();
                };
                {
                    let it = handle.scan_all(tx);
                    for t in it {
                        let t = t?;
                        let id: Vec<DataValue> = if is_bitemporal {
                            let mut id = t[..kpos - 1].to_vec();
                            match &t[kpos - 1] {
                                DataValue::Validity(v) => {
                                    id.push(DataValue::from(v.timestamp.0 .0))
                                }
                                // a silent omission here would merge adjacent
                                // vt-groups and over-delete
                                _ => bail!("corrupt vt component in {}", rel),
                            }
                            id
                        } else {
                            t[..kpos].to_vec()
                        };
                        let (tt_us, flag) = match &t[kpos] {
                            DataValue::Validity(v) => (v.timestamp.0 .0, v.is_assert.0),
                            _ => bail!("corrupt tt component in {}", rel),
                        };
                        // On bitemporal relations the op rides the VT flag
                        // (the tt flag byte is reserved-0); the tie-break
                        // below must compare the real assert/retract bit.
                        let flag = if is_bitemporal {
                            match &t[kpos - 1] {
                                DataValue::Validity(v) => v.is_assert.0,
                                _ => unreachable!("vt component checked above"),
                            }
                        } else {
                            flag
                        };
                        let key_bytes = t[..=kpos].to_vec().encode_as_key(handle.id);
                        if group_id.as_ref() != Some(&id) {
                            flush(&mut group, &mut to_delete);
                            group_id = Some(id);
                        }
                        group.push((key_bytes, tt_us, flag));
                    }
                    flush(&mut group, &mut to_delete);
                }
                for kb in to_delete {
                    tx.store_tx.del(&kb)?;
                    deleted += 1;
                }
                // Persist the gc floor on the relation metadata — but only
                // when something was deleted: after a no-op run every read
                // below the cutoff is still exact, and the floor is
                // irreversible.
                if deleted > 0 {
                    handle.tt_gc_floor = Some(handle.tt_gc_floor.unwrap_or(i64::MIN).max(cutoff));
                    let name_key =
                        vec![DataValue::Str(handle.name.clone())].encode_as_key(RelationId::SYSTEM);
                    let mut meta_val = vec![];
                    serde::Serialize::serialize(
                        &handle,
                        &mut rmp_serde::Serializer::new(&mut meta_val).with_struct_map(),
                    )
                    .unwrap();
                    tx.store_tx.put(&name_key, &meta_val)?;
                }

                Ok(NamedRows::new(
                    vec!["deleted".to_string(), "gc_floor".to_string()],
                    vec![vec![
                        DataValue::from(deleted as i64),
                        // the EFFECTIVE floor, which an older-cutoff re-run
                        // does not lower
                        match handle.tt_gc_floor {
                            Some(f) => DataValue::from(f),
                            None => DataValue::Null,
                        },
                    ]],
                ))
            }
            SysOp::TtEvict(rel, keys, unredacted) => {
                if read_only {
                    bail!("Cannot run ::evict in read-only mode");
                }
                let audit_name = SmartString::from("mnestic_evict_audit");
                let locks = if skip_locking {
                    vec![]
                } else {
                    self.obtain_relation_locks([&rel.name, &audit_name].into_iter())
                };
                let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();

                let handle = tx.get_relation(rel, true)?;
                // Graph-projection dirty-set hook (spec §3.4 row 12). The audit
                // relation this arm also writes is marked where it is resolved.
                tx.mark_dirty(&handle);
                if !handle.has_txtime() {
                    bail!("::evict requires a TxTime relation, {} is not", rel);
                }
                if handle.access_level < AccessLevel::Normal {
                    bail!(InsufficientAccessLevel(
                        handle.name.to_string(),
                        "eviction".to_string(),
                        handle.access_level
                    ))
                }
                if tx
                    .pending_tt_writes
                    .iter()
                    .any(|w| w.handle.name == handle.name)
                {
                    bail!(
                        "relation {} has pending transaction-time writes in this transaction; \
                         they would be stamped after the eviction and resurrect the evicted \
                         keys — commit them in their own transaction first",
                        rel
                    );
                }
                let kpos = handle.metadata.keys.len() - 1;
                let is_bitemporal = !handle.is_tt_only();
                let plain_len = if is_bitemporal { kpos - 1 } else { kpos };

                // salt for the audit key-hash, generated once per store
                // (locking read: two racing first-evicts must not each mint
                // a salt, or the loser's audit markers become unlinkable)
                let salt_key = {
                    let t = vec![DataValue::Null, DataValue::from("EVICT_SALT")];
                    t.encode_as_key(RelationId::SYSTEM)
                };
                let salt = match tx.store_tx.get(&salt_key, true)? {
                    Some(s) => s,
                    None => {
                        let mut buf = [0u8; 16];
                        rand::Rng::fill(&mut rand::thread_rng(), &mut buf);
                        tx.store_tx.put(&salt_key, &buf)?;
                        buf.to_vec()
                    }
                };
                let ns = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &salt);

                let audit = self.tt_evict_audit_relation(tx)?;
                // Graph-projection dirty-set hook (spec §3.4 row 12, second
                // relation): `::evict` writes audit rows into `mnestic_evict_audit`.
                tx.mark_dirty(&audit);
                let evict_tt = tx.tt_clock.advance();
                // route this tx's commit through the tt path so the burned
                // audit tt is covered by the persisted HWM (crash + clock
                // regression must not re-allocate it)
                tx.tt_hwm_dirty = true;

                let headers = vec![
                    "relation".to_string(),
                    "key".to_string(),
                    "rows_deleted".to_string(),
                    "tt".to_string(),
                ];
                let mut out_rows = Vec::new();
                let mut seen_keys = std::collections::BTreeSet::new();
                for key in keys {
                    if key.len() != plain_len {
                        bail!(
                            "::evict key arity mismatch for {}: expected {} plain key column(s), got {}",
                            rel,
                            plain_len,
                            key.len()
                        );
                    }
                    // coerce through the column types: a mistyped key must
                    // error loudly, not silently evict nothing
                    let key = key
                        .iter()
                        .zip(handle.metadata.keys.iter())
                        .map(|(v, c)| {
                            c.typing
                                .coerce(v.clone(), crate::data::functions::MAX_VALIDITY_TS)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if !seen_keys.insert(key.clone()) {
                        // a repeated key would overwrite its own audit row
                        // (same tt) with rows_deleted = 0
                        continue;
                    }
                    let mut kill: Vec<Vec<u8>> = Vec::new();
                    for found in handle.scan_prefix(tx, &key) {
                        let t = found?;
                        kill.push(t[..=kpos].to_vec().encode_as_key(handle.id));
                    }
                    let n = kill.len();
                    for kb in kill {
                        tx.store_tx.del(&kb)?;
                    }
                    // the audit marker: a salted hash by default — storing
                    // the key itself would re-enshrine the PII the eviction
                    // removes; `unredacted` opts out.
                    let key_repr = if *unredacted {
                        format!("{key:?}")
                    } else {
                        let enc = key.to_vec().encode_as_key(handle.id);
                        uuid::Uuid::new_v5(&ns, &enc).to_string()
                    };
                    let audit_row = vec![
                        DataValue::from(handle.name.to_string().as_str()),
                        DataValue::from(key_repr.as_str()),
                        DataValue::from(evict_tt),
                        DataValue::from(n as i64),
                    ];
                    let ak = audit.encode_key_for_store(&audit_row, Default::default())?;
                    let av = audit.encode_val_for_store(&audit_row, Default::default())?;
                    tx.store_tx.put(&ak, &av)?;

                    out_rows.push(vec![
                        DataValue::from(handle.name.to_string().as_str()),
                        DataValue::from(key_repr.as_str()),
                        DataValue::from(n as i64),
                        DataValue::from(evict_tt),
                    ]);
                }
                Ok(NamedRows::new(headers, out_rows))
            }
            SysOp::RemoveRelation(rel_names) => {
                if read_only {
                    bail!("Cannot remove relations in read-only mode");
                }
                let rel_name_strs = rel_names.iter().map(|n| &n.name);
                let locks = if skip_locking {
                    vec![]
                } else {
                    self.obtain_relation_locks(rel_name_strs)
                };
                let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();
                let mut bounds = vec![];
                for rs in rel_names {
                    let bound = tx.destroy_relation(rs)?;
                    if !rs.is_temp_store_name() {
                        bounds.extend(bound);
                    }
                }
                for (lower, upper) in bounds {
                    tx.store_tx.del_range_from_persisted(&lower, &upper)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::DescribeRelation(rel_name, description) => {
                if read_only {
                    bail!("Cannot describe relation in read-only mode");
                }
                tx.describe_relation(rel_name, description)?;
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::Reindex(rel_name) => {
                if read_only {
                    bail!("Cannot reindex a relation in read-only mode");
                }
                // The relation WRITE lock, held for the whole rebuild: every
                // index row is deleted and re-derived, so a concurrent reader
                // would otherwise see a half-built index. On a large relation
                // this blocks that relation for the duration — `::reindex` is a
                // maintenance operation, and its docs say so. (It is never
                // auto-invoked: the import paths point at it, they do not run
                // it.)
                let lock = self
                    .obtain_relation_locks(iter::once(&rel_name.name))
                    .pop()
                    .unwrap();
                let _guard = lock.write().unwrap();

                let handle = tx.get_relation(rel_name, true)?;
                // Graph-projection dirty-set hook: the index rows below are
                // rewritten under the projection cache's nose.
                tx.mark_dirty(&handle);

                let reports = tx.reindex_relation(&rel_name.name)?;
                if reports.is_empty() {
                    // Not an error: this keeps `::reindex` scriptable across a
                    // set of relations without the caller having to know which
                    // of them carry search indexes.
                    return Ok(NamedRows::new(
                        vec![STATUS_STR.to_string()],
                        vec![vec![DataValue::from(
                            "no HNSW/FTS/LSH index on this relation — nothing to rebuild",
                        )]],
                    ));
                }
                let (headers, rows) = crate::runtime::reindex::reindex_rows(reports);
                Ok(NamedRows::new(headers, rows))
            }
            SysOp::RepairCorrupt(rel_name) => {
                if read_only {
                    bail!("Cannot repair relation in read-only mode");
                }
                let lock = self
                    .obtain_relation_locks(iter::once(&rel_name.name))
                    .pop()
                    .unwrap();
                let _guard = lock.write().unwrap();
                let handle = tx.get_relation(rel_name, true)?;
                // Graph-projection dirty-set hook (spec §3.4 row 10).
                tx.mark_dirty(&handle);
                let expected = handle.metadata.keys.len() + handle.metadata.non_keys.len();
                let lower = Tuple::default().encode_as_key(handle.id);
                let upper = Tuple::default().encode_as_key(handle.id.next());
                // Key bytes survive value truncation (keys and values are
                // separate byte strings), so a short DECODED tuple still has
                // an intact store key we can delete by.
                let mut bad_keys: Vec<Vec<u8>> = vec![];
                for kv in tx.store_tx.range_scan(&lower, &upper) {
                    let (k, v) = kv?;
                    if decode_tuple_from_kv(&k, &v, None).len() < expected {
                        bad_keys.push(k);
                    }
                }
                let removed = bad_keys.len();
                for k in &bad_keys {
                    tx.store_tx.del(k)?;
                }
                Ok(NamedRows::new(
                    vec!["removed".to_string()],
                    vec![vec![DataValue::from(removed as i64)]],
                ))
            }
            SysOp::CreateIndex(rel_name, idx_name, cols) => {
                if read_only {
                    bail!("Cannot create index in read-only mode");
                }
                if skip_locking {
                    tx.create_index(rel_name, idx_name, cols)?;
                } else {
                    let lock = self
                        .obtain_relation_locks(iter::once(&rel_name.name))
                        .pop()
                        .unwrap();
                    let _guard = lock.write().unwrap();
                    tx.create_index(rel_name, idx_name, cols)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::CreateVectorIndex(config) => {
                if read_only {
                    bail!("Cannot create vector index in read-only mode");
                }
                if skip_locking {
                    self.build_and_publish_hnsw(tx, config)?;
                } else {
                    let lock = self
                        .obtain_relation_locks(iter::once(&config.base_relation))
                        .pop()
                        .unwrap();
                    let _guard = lock.write().unwrap();
                    self.build_and_publish_hnsw(tx, config)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::CreateFtsIndex(config) => {
                if read_only {
                    bail!("Cannot create fts index in read-only mode");
                }
                if skip_locking {
                    tx.create_fts_index(config)?;
                } else {
                    let lock = self
                        .obtain_relation_locks(iter::once(&config.base_relation))
                        .pop()
                        .unwrap();
                    let _guard = lock.write().unwrap();
                    tx.create_fts_index(config)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::CreateMinHashLshIndex(config) => {
                if read_only {
                    bail!("Cannot create minhash lsh index in read-only mode");
                }
                if skip_locking {
                    tx.create_minhash_lsh_index(config)?;
                } else {
                    let lock = self
                        .obtain_relation_locks(iter::once(&config.base_relation))
                        .pop()
                        .unwrap();
                    let _guard = lock.write().unwrap();
                    tx.create_minhash_lsh_index(config)?;
                }

                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::RemoveIndex(rel_name, idx_name) => {
                if read_only {
                    bail!("Cannot remove index in read-only mode");
                }
                let bounds = if skip_locking {
                    tx.remove_index(rel_name, idx_name)?
                } else {
                    let lock = self
                        .obtain_relation_locks(iter::once(&rel_name.name))
                        .pop()
                        .unwrap();
                    let _guard = lock.read().unwrap();
                    tx.remove_index(rel_name, idx_name)?
                };

                for (lower, upper) in bounds {
                    tx.store_tx.del_range_from_persisted(&lower, &upper)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::ListColumns(rs) => self.list_columns(tx, rs),
            SysOp::ListIndices(rs) => self.list_indices(tx, rs),
            SysOp::RenameRelation(rename_pairs) => {
                if read_only {
                    bail!("Cannot rename relations in read-only mode");
                }
                let rel_names = rename_pairs.iter().flat_map(|(f, t)| [&f.name, &t.name]);
                let locks = if skip_locking {
                    vec![]
                } else {
                    self.obtain_relation_locks(rel_names)
                };
                let _guards = locks.iter().map(|l| l.read().unwrap()).collect_vec();
                for (old, new) in rename_pairs {
                    tx.rename_relation(old, new)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::ListRunning => self.list_running(),
            SysOp::KillRunning(id) => {
                let queries = self.running_queries.lock().unwrap();
                Ok(match queries.get(id) {
                    None => NamedRows::new(
                        vec![STATUS_STR.to_string()],
                        vec![vec![DataValue::from("NOT_FOUND")]],
                    ),
                    Some(handle) => {
                        handle.poison.kill();
                        NamedRows::new(
                            vec![STATUS_STR.to_string()],
                            vec![vec![DataValue::from("KILLING")]],
                        )
                    }
                })
            }
            SysOp::ShowTrigger(name) => {
                let rel = tx.get_relation(name, false)?;
                let mut rows: Vec<Vec<JsonValue>> = vec![];
                for (i, trigger) in rel.put_triggers.iter().enumerate() {
                    rows.push(vec![json!("put"), json!(i), json!(trigger)])
                }
                for (i, trigger) in rel.rm_triggers.iter().enumerate() {
                    rows.push(vec![json!("rm"), json!(i), json!(trigger)])
                }
                for (i, trigger) in rel.replace_triggers.iter().enumerate() {
                    rows.push(vec![json!("replace"), json!(i), json!(trigger)])
                }
                let rows = rows
                    .into_iter()
                    .map(|row| row.into_iter().map(DataValue::from).collect_vec())
                    .collect_vec();
                Ok(NamedRows::new(
                    vec!["type".to_string(), "idx".to_string(), "trigger".to_string()],
                    rows,
                ))
            }
            SysOp::SetTriggers(name, puts, rms, replaces) => {
                if read_only {
                    bail!("Cannot set triggers in read-only mode");
                }
                tx.set_relation_triggers(name, puts, rms, replaces)?;
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
            SysOp::SetAccessLevel(names, level) => {
                if read_only {
                    bail!("Cannot set access level in read-only mode");
                }
                for name in names {
                    tx.set_access_level(name, *level)?;
                }
                Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ))
            }
        }
    }
    fn run_sys_op(&'s self, op: SysOp, read_only: bool) -> Result<NamedRows> {
        // ::running and ::kill touch only the in-memory query registry, so
        // they dispatch before any transaction is opened: on mem/sqlite a
        // write tx takes a store-wide lock that queues behind every running
        // read query — a ::kill would otherwise block until the query it is
        // trying to kill finishes. (mnestic fork, interruptibility; the arms
        // in run_sys_op_with_tx stay for the imperative in-script path)
        match &op {
            SysOp::ListRunning => return self.list_running(),
            SysOp::KillRunning(id) => {
                let queries = self.running_queries.lock().unwrap();
                return Ok(match queries.get(id) {
                    None => NamedRows::new(
                        vec![STATUS_STR.to_string()],
                        vec![vec![DataValue::from("NOT_FOUND")]],
                    ),
                    Some(handle) => {
                        handle.poison.kill();
                        NamedRows::new(
                            vec![STATUS_STR.to_string()],
                            vec![vec![DataValue::from("KILLING")]],
                        )
                    }
                });
            }
            _ => {}
        }
        // RocksDB builds HNSW indexes off-lock so reads aren't blocked for
        // minutes; that path manages its own transactions and locks, so it runs
        // outside the single-tx wrapper below. (mnestic fork)
        if let SysOp::CreateVectorIndex(config) = &op {
            if !read_only && self.db.supports_sst_ingest() {
                self.create_hnsw_index_nonblocking(config)?;
                return Ok(NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ));
            }
        }
        let mut tx = if read_only {
            self.transact()?
        } else {
            self.transact_write()?
        };
        let res = self.run_sys_op_with_tx(&mut tx, &op, read_only, false)?;
        tx.commit_tx()?;
        Ok(res)
    }
    /// This is the entry to query evaluation
    pub(crate) fn run_query(
        &self,
        tx: &mut SessionTx<'_>,
        input_program: InputProgram,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        top_level: bool,
    ) -> Result<(NamedRows, Vec<(Vec<u8>, Vec<u8>)>)> {
        // cleanups contain stored relations that should be deleted at the end of query
        let mut clean_ups = vec![];

        // Some checks in case the query specifies mutation
        if let Some((meta, op, _)) = &input_program.out_opts.store_relation {
            if *op == RelationOp::Create {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} conflicts with an existing one")]
                #[diagnostic(code(eval::stored_relation_conflict))]
                struct StoreRelationConflict(String);

                ensure!(
                    !tx.relation_exists(&meta.name)?,
                    StoreRelationConflict(meta.name.to_string())
                )
            } else if *op != RelationOp::Replace {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} not found")]
                #[diagnostic(code(eval::stored_relation_not_found))]
                struct StoreRelationNotFoundError(String);

                let existing = tx.get_relation(&meta.name, false)?;

                ensure!(
                    tx.relation_exists(&meta.name)?,
                    StoreRelationNotFoundError(meta.name.to_string())
                );

                existing.ensure_compatible(meta, *op)?;
            }
        };

        // query compilation
        let entry_head_or_default = input_program.get_entry_out_head_or_default()?;
        let (normalized_program, out_opts) = input_program.into_normalized_program(tx)?;
        // mnestic fork (query factorization, 0.10.5): detector advisory (always)
        // + automatic count rewrite (behind the Db kill switch, default OFF).
        // Output headers come from `entry_head_or_default` (captured above from
        // the original program), so the rewrite never changes the result schema.
        let (normalized_program, _advisory) = crate::query::factorize::maybe_rewrite_and_advise(
            normalized_program,
            self.enable_factorize.load(Ordering::Relaxed),
        );
        let (stratified_program, store_lifetimes) = normalized_program.into_stratified_program()?;
        let program = stratified_program.magic_sets_rewrite(tx)?;
        let compiled = tx.stratified_magic_compile(program)?;

        // poison terminates queries early: on `::kill` (its flag) or when the
        // effective wall-clock deadline passes (mnestic fork, query budget).
        // The deadline is the minimum of the whole-script budget carried on the
        // transaction (per-call timeout + Db default, anchored at script start,
        // inherited by triggers running in the same tx) and this block's own
        // `:timeout` (re-armed from now). `min` makes a `:timeout` a guard that
        // can only tighten the budget, never extend past the call/Db-default.
        let effective_deadline = {
            let mut deadline = tx.script_deadline;
            if let Some(secs) = out_opts.timeout {
                if let Some(block) = budget_now().and_then(|start| deadline_from_secs(start, secs)) {
                    deadline = Some(deadline.map_or(block, |existing| existing.min(block)));
                }
            }
            deadline
        };
        let poison = Poison::with_deadline(effective_deadline);
        // give the query an ID and store it so that it can be queried and cancelled
        let id = self.queries_count.fetch_add(1, Ordering::AcqRel);

        // time the query
        let since_the_epoch = seconds_since_the_epoch()?;

        let handle = RunningQueryHandle {
            started_at: since_the_epoch,
            poison: poison.clone(),
        };
        self.running_queries.lock().unwrap().insert(id, handle);

        // RAII cleanups of running query handle
        let _guard = RunningQueryCleanup {
            id,
            running_queries: self.running_queries.clone(),
        };

        let total_num_to_take = if out_opts.sorters.is_empty() {
            out_opts.num_to_take()
        } else {
            None
        };

        let num_to_skip = if out_opts.sorters.is_empty() {
            out_opts.offset
        } else {
            None
        };

        // the real evaluation
        let (result_store, early_return) = tx.stratified_magic_evaluate(
            &compiled,
            store_lifetimes,
            total_num_to_take,
            num_to_skip,
            poison,
        )?;

        // deal with assertions
        if let Some(assertion) = &out_opts.assertion {
            match assertion {
                QueryAssertion::AssertNone(span) => {
                    if let Some(tuple) = result_store.all_iter().next() {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error(
                            "The query is asserted to return no result, but a tuple {0:?} is found"
                        )]
                        #[diagnostic(code(eval::assert_none_failure))]
                        struct AssertNoneFailure(Tuple, #[label] SourceSpan);
                        bail!(AssertNoneFailure(tuple.into_tuple(), *span))
                    }
                }
                QueryAssertion::AssertSome(span) => {
                    if result_store.all_iter().next().is_none() {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error("The query is asserted to return some results, but returned none")]
                        #[diagnostic(code(eval::assert_some_failure))]
                        struct AssertSomeFailure(#[label] SourceSpan);
                        bail!(AssertSomeFailure(*span))
                    }
                }
            }
        }

        if !out_opts.sorters.is_empty() {
            // sort outputs if required
            let sorted_result =
                tx.sort_and_collect(result_store, &out_opts.sorters, &entry_head_or_default)?;
            let sorted_iter = if let Some(offset) = out_opts.offset {
                Left(sorted_result.into_iter().skip(offset))
            } else {
                Right(sorted_result.into_iter())
            };
            let sorted_iter = if let Some(limit) = out_opts.limit {
                Left(sorted_iter.take(limit))
            } else {
                Right(sorted_iter)
            };
            if let Some((meta, relation_op, returning)) = &out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        self,
                        sorted_iter,
                        *relation_op,
                        meta,
                        &entry_head_or_default,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        top_level,
                        if *returning == ReturnMutation::Returning {
                            &meta.name.name
                        } else {
                            ""
                        },
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                clean_ups.extend(to_clear);
                let returned_rows =
                    tx.get_returning_rows(callback_collector, &meta.name, returning)?;
                Ok((returned_rows, clean_ups))
            } else {
                // not sorting outputs
                let rows: Vec<Tuple> = sorted_iter.collect_vec();
                Ok((
                    NamedRows::new(
                        entry_head_or_default
                            .iter()
                            .map(|s| s.to_string())
                            .collect_vec(),
                        rows,
                    ),
                    clean_ups,
                ))
            }
        } else {
            let scan = if early_return {
                Right(Left(
                    result_store.early_returned_iter().map(|t| t.into_tuple()),
                ))
            } else if out_opts.limit.is_some() || out_opts.offset.is_some() {
                let limit = out_opts.limit.unwrap_or(usize::MAX);
                let offset = out_opts.offset.unwrap_or(0);
                Right(Right(
                    result_store
                        .all_iter()
                        .skip(offset)
                        .take(limit)
                        .map(|t| t.into_tuple()),
                ))
            } else {
                Left(result_store.all_iter().map(|t| t.into_tuple()))
            };

            if let Some((meta, relation_op, returning)) = &out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        self,
                        scan,
                        *relation_op,
                        meta,
                        &entry_head_or_default,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        top_level,
                        if *returning == ReturnMutation::Returning {
                            &meta.name.name
                        } else {
                            ""
                        },
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                clean_ups.extend(to_clear);
                let returned_rows =
                    tx.get_returning_rows(callback_collector, &meta.name, returning)?;

                Ok((returned_rows, clean_ups))
            } else {
                let rows: Vec<Tuple> = scan.collect_vec();

                Ok((
                    NamedRows::new(
                        entry_head_or_default
                            .iter()
                            .map(|s| s.to_string())
                            .collect_vec(),
                        rows,
                    ),
                    clean_ups,
                ))
            }
        }
    }
    pub(crate) fn list_running(&self) -> Result<NamedRows> {
        let rows = self
            .running_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                vec![
                    DataValue::from(*k as i64),
                    DataValue::from(format!("{:?}", v.started_at)),
                ]
            })
            .collect_vec();
        Ok(NamedRows::new(
            vec!["id".to_string(), "started_at".to_string()],
            rows,
        ))
    }
    fn list_indices(&'s self, tx: &SessionTx<'_>, name: &str) -> Result<NamedRows> {
        let handle = tx.get_relation(name, false)?;
        let mut rows = vec![];
        for (name, (rel, cols)) in &handle.indices {
            rows.push(vec![
                json!(name),
                json!("normal"),
                json!([rel.name]),
                json!({ "indices": cols }),
            ]);
        }
        for (name, (rel, manifest)) in &handle.hnsw_indices {
            rows.push(vec![
                json!(name),
                json!("hnsw"),
                json!([rel.name]),
                json!({
                    "vec_dim": manifest.vec_dim,
                    "dtype": manifest.dtype,
                    "vec_fields": manifest.vec_fields,
                    "distance": manifest.distance,
                    "ef_construction": manifest.ef_construction,
                    "m_neighbours": manifest.m_neighbours,
                    "m_max": manifest.m_max,
                    "m_max0": manifest.m_max0,
                    "level_multiplier": manifest.level_multiplier,
                    "extend_candidates": manifest.extend_candidates,
                    "keep_pruned_connections": manifest.keep_pruned_connections,
                }),
            ]);
        }
        for (name, (rel, manifest)) in &handle.fts_indices {
            rows.push(vec![
                json!(name),
                json!("fts"),
                json!([rel.name]),
                json!({
                    "extractor": manifest.extractor,
                    "tokenizer": manifest.tokenizer,
                    "tokenizer_filters": manifest.filters,
                }),
            ]);
        }
        for (name, (rel, inv_rel, manifest)) in &handle.lsh_indices {
            rows.push(vec![
                json!(name),
                json!("lsh"),
                json!([rel.name, inv_rel.name]),
                json!({
                    "extractor": manifest.extractor,
                    "tokenizer": manifest.tokenizer,
                    "tokenizer_filters": manifest.filters,
                    "n_gram": manifest.n_gram,
                    "num_perm": manifest.num_perm,
                    "n_bands": manifest.n_bands,
                    "n_rows_in_band": manifest.n_rows_in_band,
                    "threshold": manifest.threshold,
                }),
            ]);
        }
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(DataValue::from).collect_vec())
            .collect_vec();
        Ok(NamedRows::new(
            vec![
                "name".to_string(),
                "type".to_string(),
                "relations".to_string(),
                "config".to_string(),
            ],
            rows,
        ))
    }
    fn list_columns(&'s self, tx: &SessionTx<'_>, name: &str) -> Result<NamedRows> {
        let handle = tx.get_relation(name, false)?;
        let mut rows = vec![];
        let mut idx = 0;
        for col in &handle.metadata.keys {
            let default_expr = col.default_gen.as_ref().map(|gen| format!("{}", gen));

            rows.push(vec![
                json!(col.name),
                json!(true),
                json!(idx),
                json!(col.typing.to_string()),
                json!(col.default_gen.is_some()),
                json!(default_expr),
            ]);
            idx += 1;
        }
        for col in &handle.metadata.non_keys {
            let default_expr = col.default_gen.as_ref().map(|gen| format!("{}", gen));

            rows.push(vec![
                json!(col.name),
                json!(false),
                json!(idx),
                json!(col.typing.to_string()),
                json!(col.default_gen.is_some()),
                json!(default_expr),
            ]);
            idx += 1;
        }
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(DataValue::from).collect_vec())
            .collect_vec();
        Ok(NamedRows::new(
            vec![
                "column".to_string(),
                "is_key".to_string(),
                "index".to_string(),
                "type".to_string(),
                "has_default".to_string(),
                "default_expr".to_string(),
            ],
            rows,
        ))
    }
    fn list_relations(&'s self, tx: &SessionTx<'_>) -> Result<NamedRows> {
        let lower = vec![DataValue::from("")].encode_as_key(RelationId::SYSTEM);
        let upper =
            vec![DataValue::from(String::from(LARGEST_UTF_CHAR))].encode_as_key(RelationId::SYSTEM);
        let mut rows: Vec<Vec<JsonValue>> = vec![];
        for kv_res in tx.store_tx.range_scan(&lower, &upper) {
            let (k_slice, v_slice) = kv_res?;
            if upper <= k_slice {
                break;
            }
            let meta = RelationHandle::decode(&v_slice)?;
            let n_keys = meta.metadata.keys.len();
            let n_dependents = meta.metadata.non_keys.len();
            let arity = n_keys + n_dependents;
            let name = meta.name;
            let access_level = if name.contains(':') {
                "index".to_string()
            } else {
                meta.access_level.to_string()
            };
            rows.push(vec![
                json!(name),
                json!(arity),
                json!(access_level),
                json!(n_keys),
                json!(n_dependents),
                json!(meta.put_triggers.len()),
                json!(meta.rm_triggers.len()),
                json!(meta.replace_triggers.len()),
                json!(meta.description),
            ]);
        }
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(DataValue::from).collect_vec())
            .collect_vec();
        Ok(NamedRows::new(
            vec![
                "name".to_string(),
                "arity".to_string(),
                "access_level".to_string(),
                "n_keys".to_string(),
                "n_non_keys".to_string(),
                "n_put_triggers".to_string(),
                "n_rm_triggers".to_string(),
                "n_replace_triggers".to_string(),
                "description".to_string(),
            ],
            rows,
        ))
    }
}

/// Evaluate a string expression in the context of a set of parameters and variables
pub fn evaluate_expressions(
    src: &str,
    params: &BTreeMap<String, DataValue>,
    vars: &BTreeMap<String, DataValue>,
) -> Result<DataValue> {
    _evaluate_expressions(src, params, vars).map_err(|err| {
        if err.source().is_none() {
            err.with_source_code(format!("{src} "))
        } else {
            err
        }
    })
}

/// Get the variables referenced in a string expression
pub fn get_variables(src: &str, params: &BTreeMap<String, DataValue>) -> Result<BTreeSet<String>> {
    _get_variables(src, params).map_err(|err| {
        if err.source().is_none() {
            err.with_source_code(format!("{src} "))
        } else {
            err
        }
    })
}

fn _evaluate_expressions(
    src: &str,
    params: &BTreeMap<String, DataValue>,
    vars: &BTreeMap<String, DataValue>,
) -> Result<DataValue> {
    let mut expr = parse_expressions(src, params)?;
    let mut ctx = vec![];
    let mut binding_map = BTreeMap::new();
    for (i, (k, v)) in vars.iter().enumerate() {
        ctx.push(v.clone());
        binding_map.insert(Symbol::new(k, Default::default()), i);
    }
    expr.fill_binding_indices(&binding_map)?;
    expr.eval(&ctx)
}

fn _get_variables(src: &str, params: &BTreeMap<String, DataValue>) -> Result<BTreeSet<String>> {
    let expr = parse_expressions(src, params)?;
    expr.get_variables()
}

/// Used for user-initiated termination of running queries (`::kill`) and
/// per-query wall-clock budgets (mnestic fork, query budget). Two independent
/// trip conditions, distinguished by the error they raise:
///
/// * `flag` — set by `::kill`; raises `eval::killed`.
/// * `deadline` — a monotonic instant computed from the effective budget
///   (a `:timeout` option, a per-call timeout, or the Db default); raises the
///   distinct `eval::timeout` once `Instant::now()` reaches it.
///
/// Cheap to clone (an `Arc` handle plus a `Copy` deadline), so it is threaded
/// into fixed rules and the RA enumeration pipeline. The carried deadline
/// replaces the old detached timer thread: no per-query thread leak, and
/// `:timeout` no longer depends on spawning a thread.
#[derive(Clone, Default)]
pub struct Poison {
    pub(crate) flag: Arc<AtomicBool>,
    pub(crate) deadline: Option<Instant>,
}

/// A monotonic clock reading for a wall-clock query budget, or `None` where no
/// Warn when a bulk-load path is about to write rows underneath HNSW/FTS/LSH
/// indexes it does not maintain (mnestic fork, hardening).
///
/// Both bulk paths — `import_relations` and `import_from_backup` — bypass the
/// per-row `:put` path that maintains those indexes, so the imported rows are
/// silently invisible to vector/text/LSH search until the index is rebuilt.
/// `import_relations` has warned since the fork's early hardening pass;
/// `import_from_backup` never did (0.12.1 fix), which is why this lives in one
/// place: the two must not drift again. B-tree secondary indexes are unaffected
/// — both paths maintain those.
///
/// A warning rather than a hard error, deliberately: import-then-rebuild is a
/// legitimate and common flow, and refusing it would break the very callers who
/// are doing the right thing. `verb` names the operation for the message
/// ("bulk import into", "backup restore into").
fn warn_if_indexes_stranded(handle: &RelationHandle, relation: &str, verb: &str) {
    let mut kinds = vec![];
    if !handle.hnsw_indices.is_empty() {
        kinds.push("HNSW");
    }
    if !handle.fts_indices.is_empty() {
        kinds.push("FTS");
    }
    if !handle.lsh_indices.is_empty() {
        kinds.push("LSH");
    }
    if kinds.is_empty() {
        return;
    }
    log::warn!(
        "{} relation '{}' does not maintain its {} index(es); the imported rows are \
         invisible to those indices until you rebuild them — run `::reindex {}`",
        verb,
        relation,
        kinds.join("/"),
        relation
    );
}

/// monotonic clock is available: wasm has no `std` monotonic `Instant`, so on
/// that target queries simply carry no time budget rather than panicking on
/// `Instant::now()` (mnestic fork, query budget).
#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn budget_now() -> Option<Instant> {
    Some(Instant::now())
}
#[cfg(target_arch = "wasm32")]
#[inline]
fn budget_now() -> Option<Instant> {
    None
}

/// The deadline `start + secs`, or `None` if `secs` is non-positive/non-finite
/// or the addition would overflow `Instant`. Saturating cast + `checked_add`,
/// so an infinite or absurdly large user-supplied budget (`:timeout 1e300`, an
/// HTTP `timeout` of `1e400`) yields "no deadline" — still interruptible via
/// `::kill` — instead of panicking. (mnestic fork, query budget)
fn deadline_from_secs(start: Instant, secs: f64) -> Option<Instant> {
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    // float→int casts saturate in Rust, so a huge finite `secs` clamps to
    // u64::MAX ms (~5.8e8 years) and `checked_add` then reports the overflow.
    let ms = (secs * 1000.0) as u64;
    start.checked_add(Duration::from_millis(ms))
}

impl Poison {
    /// Returns `Err` if the query has been killed (`eval::killed`) or has
    /// exceeded its wall-clock budget (`eval::timeout`). Cheap: an atomic load,
    /// plus one monotonic clock read only when a deadline is armed — safe at
    /// the batched (every-`POISON_CHECK_INTERVAL`-pulls) cadence it is called
    /// at, but keep it off any per-tuple inline path.
    #[inline(always)]
    pub fn check(&self) -> Result<()> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("Running query is killed before completion")]
        #[diagnostic(code(eval::killed))]
        #[diagnostic(help("The query was killed by an explicit `::kill` command"))]
        struct ProcessKilled;

        if self.flag.load(Ordering::Relaxed) {
            bail!(ProcessKilled)
        }

        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Query exceeded its time budget")]
                #[diagnostic(code(eval::timeout))]
                #[diagnostic(help(
                    "The query ran past its wall-clock budget, set by a `:timeout` \
                     option, a per-call timeout, or the Db default query timeout. \
                     Narrow the query or raise the budget."
                ))]
                struct QueryTimeout;
                bail!(QueryTimeout)
            }
        }
        Ok(())
    }

    /// Mark the query as killed (`::kill`); a subsequent [`Poison::check`]
    /// raises `eval::killed`.
    #[inline]
    pub(crate) fn kill(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Construct a poison carrying an optional wall-clock deadline (mnestic
    /// fork, query budget). The kill flag starts un-set.
    pub(crate) fn with_deadline(deadline: Option<Instant>) -> Self {
        Poison {
            flag: Arc::new(AtomicBool::new(false)),
            deadline,
        }
    }
}

pub(crate) fn seconds_since_the_epoch() -> Result<f64> {
    #[cfg(not(target_arch = "wasm32"))]
    let now = SystemTime::now();
    #[cfg(not(target_arch = "wasm32"))]
    return Ok(now
        .duration_since(UNIX_EPOCH)
        .into_diagnostic()?
        .as_secs_f64());

    #[cfg(target_arch = "wasm32")]
    Ok(js_sys::Date::now())
}
