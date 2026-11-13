/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
#[cfg(feature = "columnar-io")]
use std::time::Duration;

use miette::{IntoDiagnostic, Report, Result};
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyDict, PyList, PyString, PyTuple};
use serde_json::json;

use cozo::*;

mod governed;
use governed::GovernedTx;

fn py_to_rows(ob: &PyAny) -> PyResult<Vec<Vec<DataValue>>> {
    let rows = ob.extract::<Vec<Vec<&PyAny>>>()?;
    let res: Vec<Vec<DataValue>> = rows
        .into_iter()
        .map(|row| row.into_iter().map(py_to_value).collect::<PyResult<_>>())
        .collect::<PyResult<_>>()?;
    Ok(res)
}

fn report2py(r: Report) -> PyErr {
    PyException::new_err(r.to_string())
}

pyo3::create_exception!(
    mnestic,
    MnesticError,
    PyException,
    "Raised for query, transaction and storage errors (mnestic 0.19.2+).\n\n\
     A subclass of Exception, so existing handlers still catch it. `args[0]` is the \
     diagnostic dict earlier releases raised. Stable attributes: `code` (diagnostic \
     code or None), `kind` (\"parse\", \"query\", \"timeout\", \"conflict\", \"limit\" \
     or \"other\"), `retryable` (True only when rerunning the same call can succeed: \
     a lock or transaction conflict), `maybe_committed` and `message`.\n\n\
     For a governed transaction (`CozoDbPy.governed_transact`, 0.19.3), `retryable` \
     refers to rerunning the whole transaction, and is True only when the \
     transaction certainly did not commit. `maybe_committed` is True when \
     `commit()` failed in a way that leaves the outcome unknown (a timeout, a \
     cancellation or a storage error during the commit): reconcile before \
     retrying. It is False on every other error, including every non-governed \
     one. `retryable` and `maybe_committed` are never both True."
);

/// Build a binding-made `MnesticError` with the same attributes and `args[0]`
/// shape as one converted from an engine diagnostic.
fn binding_error(
    py: Python<'_>,
    code: &str,
    message: &str,
    kind: &str,
    retryable: bool,
) -> PyErr {
    let build = || -> PyResult<PyErr> {
        let payload = PyDict::new_bound(py);
        payload.set_item("ok", false)?;
        payload.set_item("code", code)?;
        payload.set_item("message", message)?;
        payload.set_item("severity", "error")?;
        payload.set_item("display", format!("{code}\n\n  × {message}\n"))?;
        let err = MnesticError::new_err(payload.unbind());
        let value = err.value_bound(py);
        value.setattr("code", code)?;
        value.setattr("kind", kind)?;
        value.setattr("retryable", retryable)?;
        value.setattr("maybe_committed", false)?;
        value.setattr("message", message)?;
        Ok(err)
    };
    build().unwrap_or_else(|e| e)
}

const TX_CLOSED_CODE: &str = "transaction::closed";
const GOVERNED_WRITE_OPEN_CODE: &str = "transaction::governed_write_open";

fn tx_closed_error(py: Python<'_>) -> PyErr {
    binding_error(
        py,
        TX_CLOSED_CODE,
        "Governed transaction is closed",
        "other",
        false,
    )
}

/// Map a diagnostic to the stable `kind`/`retryable` pair on `MnesticError`.
fn classify_error(code: &str, message: &str) -> (&'static str, bool) {
    let m = message.to_ascii_lowercase();
    if code == "eval::killed" || code.contains("timeout") {
        ("timeout", false)
    } else if m.contains("database is locked")
        || m.contains("resource busy")
        || m.contains("busy")
        || m.contains("try again")
        || m.contains("deadlock")
        || m.contains("conflict")
        || (m.contains("timed out") && m.contains("lock"))
    {
        ("conflict", true)
    } else if code.starts_with("parser") || code.starts_with("parse") {
        ("parse", false)
    } else if code.contains("limit") || code.contains("budget") {
        ("limit", false)
    } else if code.starts_with("eval")
        || code.starts_with("query")
        || code.starts_with("compile")
        || code.starts_with("fixed_rule")
    {
        ("query", false)
    } else {
        ("other", false)
    }
}

/// Convert an engine error into `MnesticError`, carrying the diagnostic dict.
fn script_error(py: Python<'_>, err: Report, source: Option<&str>) -> PyErr {
    let report = format_error_as_json(err, source);
    let code = report
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message = report
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (kind, retryable) = classify_error(&code, &message);
    let build = || -> PyResult<PyErr> {
        let payload = py
            .import_bound("json")?
            .getattr("loads")?
            .call1((report.to_string(),))?;
        let err = MnesticError::new_err(payload.unbind());
        let value = err.value_bound(py);
        if code.is_empty() {
            value.setattr("code", py.None())?;
        } else {
            value.setattr("code", code.as_str())?;
        }
        value.setattr("kind", kind)?;
        value.setattr("retryable", retryable)?;
        value.setattr("maybe_committed", false)?;
        value.setattr("message", message.as_str())?;
        Ok(err)
    };
    build().unwrap_or_else(|e| e)
}

fn py_to_named_rows(ob: &PyAny) -> PyResult<NamedRows> {
    let d = ob.downcast::<PyDict>()?;
    let rows = d
        .get_item("rows")?
        .ok_or_else(|| PyException::new_err("named rows must contain 'rows'"))?;
    let rows = py_to_rows(rows)?;
    let headers = d
        .get_item("headers")?
        .ok_or_else(|| PyException::new_err("named rows must contain 'headers'"))?;
    let headers = headers.extract::<Vec<String>>()?;
    Ok(NamedRows::new(headers, rows))
}

fn py_to_value(ob: &PyAny) -> PyResult<DataValue> {
    Ok(if ob.is_none() {
        DataValue::Null
    } else if let Ok(b) = ob.downcast::<PyBool>() {
        DataValue::from(b.is_true())
    } else if let Ok(i) = ob.extract::<i64>() {
        DataValue::from(i)
    } else if let Ok(f) = ob.extract::<f64>() {
        DataValue::from(f)
    } else if let Ok(s) = ob.extract::<String>() {
        DataValue::from(s)
    } else if let Ok(b) = ob.downcast::<PyBytes>() {
        DataValue::Bytes(b.as_bytes().to_vec())
    } else if let Ok(b) = ob.downcast::<PyByteArray>() {
        DataValue::Bytes(unsafe { b.as_bytes() }.to_vec())
    } else if let Ok(l) = ob.downcast::<PyTuple>() {
        let mut coll = Vec::with_capacity(l.len());
        for el in l {
            let el = py_to_value(el)?;
            coll.push(el)
        }
        DataValue::List(coll)
    } else if let Ok(l) = ob.downcast::<PyList>() {
        let mut coll = Vec::with_capacity(l.len());
        for el in l {
            let el = py_to_value(el)?;
            coll.push(el)
        }
        DataValue::List(coll)
    } else if let Ok(d) = ob.downcast::<PyDict>() {
        let mut coll = serde_json::Map::default();
        for (k, v) in d {
            let k = serde_json::Value::from(py_to_value(k)?);
            let k = match k {
                serde_json::Value::String(s) => s,
                s => s.to_string(),
            };
            let v = serde_json::Value::from(py_to_value(v)?);
            coll.insert(k, v);
        }
        DataValue::Json(JsonData(json!(coll)))
    } else {
        return Err(PyException::new_err(format!(
            "Cannot convert {ob} into Cozo value"
        )));
    })
}

fn convert_params(ob: &PyDict) -> PyResult<BTreeMap<String, DataValue>> {
    let mut ret = BTreeMap::new();
    for (k, v) in ob {
        let k: String = k.extract()?;
        let v = py_to_value(v)?;
        ret.insert(k, v);
    }
    Ok(ret)
}

/// Build a [`HybridSearch`] from a Python dict, applying the Rust `Default` for
/// any omitted field (mnestic fork). Mirrors the field names of the Rust struct.
#[cfg(feature = "cypher")]
fn cy_req_str(d: &PyDict, k: &str) -> PyResult<String> {
    d.get_item(k)?
        .ok_or_else(|| PyException::new_err(format!("schema entry needs '{k}'")))?
        .extract()
}

#[cfg(feature = "cypher")]
fn cy_opt_str(d: &PyDict, k: &str) -> PyResult<Option<String>> {
    Ok(match d.get_item(k)? {
        Some(x) if !x.is_none() => Some(x.extract()?),
        _ => None,
    })
}

#[cfg(feature = "cypher")]
fn cy_opt_val(d: &PyDict, k: &str) -> PyResult<Option<DataValue>> {
    Ok(match d.get_item(k)? {
        Some(x) if !x.is_none() => Some(py_to_value(x)?),
        _ => None,
    })
}

/// Build a `CypherGraphSchema` from a Python dict mirroring the Rust fields:
/// `{"nodes": [{label, relation, id_col, label_col?, label_value?, filter?}, ...],
///   "edges": [{rel_type, relation, from_col, to_col, type_col?, type_value?, eid_col?, filter?}, ...]}`.
#[cfg(feature = "cypher")]
fn py_to_cypher_schema(d: &PyDict) -> PyResult<CypherGraphSchema> {
    let mut schema = CypherGraphSchema::default();
    if let Some(v) = d.get_item("nodes")? {
        for it in v.downcast::<PyList>()? {
            let nd = it.downcast::<PyDict>()?;
            schema.nodes.push(NodeMap {
                label: cy_req_str(nd, "label")?,
                relation: cy_req_str(nd, "relation")?,
                id_col: cy_req_str(nd, "id_col")?,
                label_col: cy_opt_str(nd, "label_col")?,
                label_value: cy_opt_val(nd, "label_value")?,
                filter: cy_opt_str(nd, "filter")?,
            });
        }
    }
    if let Some(v) = d.get_item("edges")? {
        for it in v.downcast::<PyList>()? {
            let ed = it.downcast::<PyDict>()?;
            schema.edges.push(EdgeMap {
                rel_type: cy_req_str(ed, "rel_type")?,
                relation: cy_req_str(ed, "relation")?,
                from_col: cy_req_str(ed, "from_col")?,
                to_col: cy_req_str(ed, "to_col")?,
                type_col: cy_opt_str(ed, "type_col")?,
                type_value: cy_opt_val(ed, "type_value")?,
                eid_col: cy_opt_str(ed, "eid_col")?,
                filter: cy_opt_str(ed, "filter")?,
            });
        }
    }
    Ok(schema)
}

fn py_to_hybrid_search(d: &PyDict) -> PyResult<HybridSearch> {
    let mut q = HybridSearch::default();
    if let Some(v) = d.get_item("relation")? {
        q.relation = v.extract()?;
    }
    if let Some(v) = d.get_item("id_col")? {
        q.id_col = v.extract()?;
    }
    if let Some(v) = d.get_item("vector_index")? {
        q.vector_index = v.extract()?;
    }
    if let Some(v) = d.get_item("query_vector")? {
        q.query_vector = v.extract()?;
    }
    if let Some(v) = d.get_item("vector_f64")? {
        q.vector_f64 = v.extract()?;
    }
    if let Some(v) = d.get_item("vector_k")? {
        q.vector_k = v.extract()?;
    }
    if let Some(v) = d.get_item("ef")? {
        q.ef = v.extract()?;
    }
    if let Some(v) = d.get_item("fts_index")? {
        q.fts_index = v.extract()?;
    }
    if let Some(v) = d.get_item("query_text")? {
        q.query_text = v.extract()?;
    }
    if let Some(v) = d.get_item("fts_k")? {
        q.fts_k = v.extract()?;
    }
    if let Some(v) = d.get_item("rrf_k")? {
        q.rrf_k = v.extract()?;
    }
    if let Some(v) = d.get_item("limit")? {
        q.limit = v.extract()?;
    }
    if let Some(v) = d.get_item("detailed")? {
        q.detailed = v.extract()?;
    }
    if let Some(v) = d.get_item("extra_lists")? {
        let items = v.downcast::<PyList>()?;
        let mut lists = Vec::with_capacity(items.len());
        for it in items {
            let ld = it.downcast::<PyDict>()?;
            let label = ld
                .get_item("label")?
                .ok_or_else(|| PyException::new_err("extra_lists entry needs 'label'"))?
                .extract()?;
            let rule_body = ld
                .get_item("rule_body")?
                .ok_or_else(|| PyException::new_err("extra_lists entry needs 'rule_body'"))?
                .extract()?;
            lists.push(HybridList { label, rule_body });
        }
        q.extra_lists = lists;
    }
    if let Some(v) = d.get_item("graph_legs")? {
        let items = v.downcast::<PyList>()?;
        let mut legs = Vec::with_capacity(items.len());
        for it in items {
            let gd = it.downcast::<PyDict>()?;
            // Every KNOWN key is optional and defaults like the Rust struct;
            // the builder's validation owns the invariants (which fields each
            // mode requires) so the rules live in exactly one place. Unknown
            // keys are REJECTED: with all keys optional, a typo ('max_hop',
            // 'seed_from_leg', a trailing space) would otherwise silently run
            // the leg with defaults — plausible results, wrong depth/mode, no
            // signal anywhere.
            const KNOWN_LEG_KEYS: [&str; 15] = [
                "label",
                "edge_relation",
                "from_col",
                "to_col",
                "seeds",
                "max_hops",
                "undirected",
                "max_nodes",
                "max_cost",
                "weight_col",
                "graph",
                "seed_from_legs",
                "gate_relation",
                "gate_cols",
                "admit",
            ];
            for key in gd.keys() {
                let key: String = key.extract()?;
                if !KNOWN_LEG_KEYS.contains(&key.as_str()) {
                    return Err(PyException::new_err(format!(
                        "graph_legs entry has unknown key {key:?}; known keys: {}",
                        KNOWN_LEG_KEYS.join(", ")
                    )));
                }
            }
            let mut leg = GraphLeg::default();
            if let Some(x) = gd.get_item("label")? {
                leg.label = x.extract()?;
            }
            if let Some(x) = gd.get_item("edge_relation")? {
                leg.edge_relation = x.extract()?;
            }
            if let Some(x) = gd.get_item("from_col")? {
                leg.from_col = x.extract()?;
            }
            if let Some(x) = gd.get_item("to_col")? {
                leg.to_col = x.extract()?;
            }
            if let Some(x) = gd.get_item("seeds")? {
                let seeds_list = x.downcast::<PyList>()?;
                let mut seeds = Vec::with_capacity(seeds_list.len());
                for s in seeds_list {
                    seeds.push(py_to_value(s)?);
                }
                leg.seeds = seeds;
            }
            if let Some(x) = gd.get_item("max_hops")? {
                leg.max_hops = x.extract()?;
            }
            if let Some(x) = gd.get_item("undirected")? {
                leg.undirected = x.extract()?;
            }
            // Budgeted-expansion mode (0.13.0): presence of max_nodes
            // switches the leg; the rest configure it.
            if let Some(x) = gd.get_item("max_nodes")? {
                leg.max_nodes = x.extract()?;
            }
            if let Some(x) = gd.get_item("max_cost")? {
                leg.max_cost = x.extract()?;
            }
            if let Some(x) = gd.get_item("weight_col")? {
                leg.weight_col = x.extract()?;
            }
            if let Some(x) = gd.get_item("graph")? {
                leg.graph = x.extract()?;
            }
            if let Some(x) = gd.get_item("seed_from_legs")? {
                leg.seed_from_legs = x.extract()?;
            }
            if let Some(x) = gd.get_item("gate_relation")? {
                leg.gate_relation = x.extract()?;
            }
            if let Some(x) = gd.get_item("gate_cols")? {
                leg.gate_cols = x.extract()?;
            }
            if let Some(x) = gd.get_item("admit")? {
                leg.admit = x.extract()?;
            }
            legs.push(leg);
        }
        q.graph_legs = legs;
    }
    if let Some(v) = d.get_item("mmr")? {
        if !v.is_none() {
            let md = v.downcast::<PyDict>()?;
            let mut m = MmrParams::default();
            if let Some(x) = md.get_item("lambda")? {
                m.lambda = x.extract()?;
            }
            if let Some(x) = md.get_item("k")? {
                m.k = x.extract()?;
            }
            if let Some(x) = md.get_item("embedding_col")? {
                m.embedding_col = x.extract()?;
            }
            q.mmr = Some(m);
        }
    }
    Ok(q)
}

fn options_to_py(opts: BTreeMap<String, DataValue>, py: Python<'_>) -> PyResult<PyObject> {
    let ret = PyDict::new(py);

    for (k, v) in opts {
        let val = value_to_py(v, py);
        ret.set_item(k, val)?;
    }
    Ok(ret.into())
}

fn json_to_py(val: serde_json::Value, py: Python<'_>) -> PyObject {
    match val {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(b) => b.into_py(py),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py(py)
            } else if let Some(f) = n.as_f64() {
                f.into_py(py)
            } else {
                py.None()
            }
        }
        serde_json::Value::String(s) => s.into_py(py),
        serde_json::Value::Array(a) => {
            let vs: Vec<_> = a.into_iter().map(|v| json_to_py(v, py)).collect();
            vs.into_py(py)
        }
        serde_json::Value::Object(o) => {
            let d = PyDict::new(py);
            for (k, v) in o {
                d.set_item(k, json_to_py(v, py)).unwrap();
            }
            d.into()
        }
    }
}

fn value_to_py(val: DataValue, py: Python<'_>) -> PyObject {
    match val {
        DataValue::Null => py.None(),
        DataValue::Bool(b) => b.into_py(py),
        DataValue::Num(num) => match num {
            Num::Int(i) => i.into_py(py),
            Num::Float(f) => f.into_py(py),
        },
        DataValue::Str(s) => s.as_str().into_py(py),
        DataValue::Bytes(b) => PyBytes::new(py, &b).into(),
        DataValue::Uuid(uuid) => uuid.0.to_string().into_py(py),
        DataValue::Regex(rx) => rx.0.as_str().into_py(py),
        DataValue::List(l) => {
            let vs: Vec<_> = l.into_iter().map(|v| value_to_py(v, py)).collect();
            vs.into_py(py)
        }
        DataValue::Set(l) => {
            let vs: Vec<_> = l.into_iter().map(|v| value_to_py(v, py)).collect();
            vs.into_py(py)
        }
        DataValue::Validity(vld) => {
            [vld.timestamp.0 .0.into_py(py), vld.is_assert.0.into_py(py)].into_py(py)
        }
        DataValue::Bot => py.None(),
        DataValue::Vec(v) => match v {
            Vector::F32(a) => {
                let vs: Vec<_> = a.into_iter().map(|v| v.into_py(py)).collect();
                vs.into_py(py)
            }
            Vector::F64(a) => {
                let vs: Vec<_> = a.into_iter().map(|v| v.into_py(py)).collect();
                vs.into_py(py)
            }
        },
        DataValue::Json(JsonData(j)) => json_to_py(j, py),
    }
}

fn rows_to_py_rows(rows: Vec<Vec<DataValue>>, py: Python<'_>) -> PyObject {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|val| value_to_py(val, py))
                .collect::<Vec<_>>()
                .into_py(py)
        })
        .collect::<Vec<_>>()
        .into_py(py)
}

fn named_rows_to_py(named_rows: NamedRows, py: Python<'_>) -> PyObject {
    let rows = rows_to_py_rows(named_rows.rows, py);
    let headers = named_rows.headers.into_py(py);
    let next = match named_rows.next {
        None => py.None(),
        Some(nxt) => named_rows_to_py(*nxt, py),
    };
    BTreeMap::from([("rows", rows), ("headers", headers), ("next", next)]).into_py(py)
}

// Interior mutability so that `close()` can take `&self` (mnestic fork, item D).
// PyO3's PyCell runtime borrow check would raise "Already borrowed" if `close`
// took `&mut self` while a concurrent `run_script(&self)` held a shared borrow
// across its GIL-released blocking call. An `RwLock` (NOT a `RefCell`, which is
// not `Sync` and cannot back a shareable pyclass) lets every method take `&self`:
// read paths clone the cheap Arc-backed `DbInstance` out of a *momentary* guard
// and drop the guard before the blocking engine call; `close` takes a write
// guard and `take()`s. Concurrent readers never block `close`, and `close` never
// waits out an in-flight query.
#[pyclass]
struct CozoDbPy {
    db: RwLock<Option<DbInstance>>,
    /// Open governed transactions of this handle (python-governed-transactions D2).
    governed: Arc<governed::Registry>,
    /// SQLite handles refuse other calls while a governed write is open (D9).
    sqlite: bool,
}

#[pyclass]
struct CozoDbMulTx {
    tx: MultiTransaction,
}

const DB_CLOSED_MSG: &str = r##"{"ok":false,"message":"database closed"}"##;

impl CozoDbPy {
    /// Clone the Arc-backed [`DbInstance`] out of a momentary read guard and
    /// drop the guard before returning, so callers never hold the lock across
    /// the blocking engine call they run under `py.allow_threads`. Returns a
    /// clear Python error (never panics) if the handle has already been closed.
    fn db_ref(&self) -> PyResult<DbInstance> {
        self.db
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| PyException::new_err(DB_CLOSED_MSG))
    }

    /// D9: on SQLite a governed write holds the store lock for its whole
    /// life, so any other storage call on this handle would wait until the
    /// transaction's idle limit rolls it back. Refuse at once instead.
    fn refuse_if_governed_write(&self, py: Python<'_>) -> PyResult<()> {
        if self.sqlite && self.governed.has_write() {
            return Err(binding_error(
                py,
                GOVERNED_WRITE_OPEN_CODE,
                "A governed write transaction opened from this handle is still open; on SQLite it excludes every other query on the handle. Commit or abort it first.",
                "conflict",
                false,
            ));
        }
        Ok(())
    }

    /// Like [`Self::db_ref`], plus the D9 refusal.
    fn db_for_storage(&self, py: Python<'_>) -> PyResult<DbInstance> {
        let db = self.db_ref()?;
        self.refuse_if_governed_write(py)?;
        Ok(db)
    }

    fn open_governed(
        &self,
        py: Python<'_>,
        write: bool,
        deadline: std::time::Instant,
        idle_timeout: std::time::Duration,
        mem_limit: Option<usize>,
    ) -> PyResult<GovernedTx> {
        let db = self.db_for_storage(py)?;
        let tx = governed::open(
            py,
            db,
            &self.governed,
            write,
            deadline,
            idle_timeout,
            mem_limit,
        )?;
        // A close() that raced this open must still end the transaction.
        if self.db.read().unwrap().is_none() {
            self.governed.cancel_all();
        }
        Ok(tx)
    }
}

#[pymethods]
impl CozoDbPy {
    #[new]
    fn new(engine: &str, path: &str, options: &str) -> PyResult<Self> {
        match DbInstance::new(engine, path, options) {
            Ok(db) => {
                #[cfg(feature = "storage-sqlite")]
                let sqlite = matches!(db, DbInstance::Sqlite(_));
                #[cfg(not(feature = "storage-sqlite"))]
                let sqlite = false;
                Ok(Self {
                    db: RwLock::new(Some(db)),
                    governed: Default::default(),
                    sqlite,
                })
            }
            Err(err) => Err(PyException::new_err(format!("{err:?}"))),
        }
    }
    /// Run a CozoScript query. `timeout` (mnestic fork, item C — query budget)
    /// is an optional per-call wall-clock budget in seconds; when set the query
    /// is run through [`DbInstance::run_script_with_options`] and a budget expiry
    /// surfaces as an `eval::timeout` error (distinct from `::kill`'s
    /// `eval::killed`). It is appended last so existing positional/keyword
    /// callers are unaffected.
    #[pyo3(signature = (query, params, immutable, timeout=None))]
    pub fn run_script(
        &self,
        py: Python<'_>,
        query: &str,
        params: &PyDict,
        immutable: bool,
        timeout: Option<f64>,
    ) -> PyResult<PyObject> {
        // Clone the handle out of a momentary read guard and drop the guard
        // BEFORE `py.allow_threads` — holding the lock across the blocking call
        // would serialize queries and re-block `close`.
        let db = self.db_for_storage(py)?;
        let params = convert_params(params)?;
        let result = py.allow_threads(|| {
            let mutability = if immutable {
                ScriptMutability::Immutable
            } else {
                ScriptMutability::Mutable
            };
            if timeout.is_some() {
                // `..Default::default()` is intentional forward-proofing per the
                // `ScriptRunOptions` doc (new options land without touching this
                // call site); it is a no-op only while `timeout` is the sole field.
                #[allow(clippy::needless_update)]
                let opts = ScriptRunOptions {
                    timeout,
                    ..Default::default()
                };
                db.run_script_with_options(query, params, mutability, opts)
            } else {
                db.run_script(query, params, mutability)
            }
        });
        match result {
            Ok(rows) => Ok(named_rows_to_py(rows, py)),
            Err(err) => {
                Err(script_error(py, err, Some(query)))
            }
        }
    }
    /// Set (or clear, with `None`) the Db-level default per-query wall-clock
    /// budget in seconds (mnestic fork, item C). Forwards to
    /// [`DbInstance::set_default_query_timeout`]. The effective budget for each
    /// statement is the minimum of this default, any per-call `timeout`, and any
    /// in-script `:timeout` — so this is a guard a query cannot escape.
    pub fn set_default_query_timeout(&self, secs: Option<f64>) -> PyResult<()> {
        let db = self.db_ref()?;
        db.set_default_query_timeout(secs);
        Ok(())
    }
    /// Read the Db-level default per-query wall-clock budget in seconds, or
    /// `None` if unset (mnestic fork, item C). Forwards to
    /// [`DbInstance::default_query_timeout`].
    pub fn default_query_timeout(&self) -> PyResult<Option<f64>> {
        let db = self.db_ref()?;
        Ok(db.default_query_timeout())
    }
    /// Enable or disable the automatic factorized-`count()` rewrite (mnestic
    /// fork, query factorization). This is a Db-wide kill switch, **default ON
    /// since 0.13.1** — when on, a `count()` over an alpha-acyclic all-positive
    /// join is computed per-separator instead of materializing the full join,
    /// without changing the result. Forwards to
    /// [`DbInstance::set_query_factorization`]. Mirrors the Rust switch so
    /// Python callers can toggle it exactly like `set_default_query_timeout`.
    pub fn set_query_factorization(&self, enabled: bool) -> PyResult<()> {
        let db = self.db_ref()?;
        db.set_query_factorization(enabled);
        Ok(())
    }
    /// Whether the automatic factorized-`count()` rewrite is currently enabled
    /// (mnestic fork, query factorization). Forwards to
    /// [`DbInstance::query_factorization`].
    pub fn query_factorization(&self) -> PyResult<bool> {
        let db = self.db_ref()?;
        Ok(db.query_factorization())
    }
    /// Request that write transactions **fsync on commit** (mnestic fork,
    /// 0.14.0). Returns `True` iff the storage backend honors the knob —
    /// `rocksdb` does (per-transaction `WriteOptions.sync`; note the bulk
    /// import/restore channels are separate non-transactional paths outside
    /// it); `sqlite` returns `False` because its writes are **already
    /// durable** (`synchronous=FULL` default); `mem` has nothing to sync.
    /// The contract this makes explicit: by default the RocksDB backend
    /// writes its WAL without fsync, so an OS crash or power loss can lose
    /// the most recent acknowledged commits (a mere process crash cannot).
    /// Forwards to [`DbInstance::set_durable_writes`].
    pub fn set_durable_writes(&self, durable: bool) -> PyResult<bool> {
        let db = self.db_ref()?;
        Ok(db.set_durable_writes(durable))
    }
    /// One-call hybrid retrieval (mnestic fork): HNSW + FTS (+ optional extra
    /// ranked lists and graph legs) fused with RRF and optionally diversified
    /// with MMR. Takes a dict mirroring the Rust `HybridSearch` fields;
    /// returns the same shape as `run_script` (`{rows, headers, next}`) with
    /// headers `["id","score"]` (or `["id","rank"]` when MMR is set). With
    /// `detailed: True` the output is long-format per-leg contributions:
    /// headers `["id","score","list_id","leg_rank","leg_score"]` (no MMR) or
    /// `["id","rank","score","list_id","leg_rank","leg_score"]` (MMR) — and
    /// when any graph leg sets `max_nodes` (budgeted mode), TWO further
    /// trailing columns `parent`, `depth` carry the cheapest-path witness
    /// (null on non-graph rows): 7 columns without MMR, 8 with it.
    pub fn hybrid_search(&self, py: Python<'_>, query: &PyDict) -> PyResult<PyObject> {
        let db = self.db_for_storage(py)?;
        let q = py_to_hybrid_search(query)?;
        match py.allow_threads(|| db.hybrid_search(&q)) {
            Ok(rows) => Ok(named_rows_to_py(rows, py)),
            Err(err) => {
                Err(script_error(py, err, None))
            }
        }
    }
    /// Run a read-only Cypher query (mnestic fork; built with the `cypher`
    /// feature). `query` is openCypher (subset); `schema` is a dict mapping the
    /// property graph onto stored relations (see `py_to_cypher_schema`); `params`
    /// supplies any `$name` parameters. Returns `{rows, headers, next}` like
    /// `run_script`, with headers = the RETURN columns.
    #[cfg(feature = "cypher")]
    pub fn run_cypher(
        &self,
        py: Python<'_>,
        query: &str,
        schema: &PyDict,
        params: &PyDict,
    ) -> PyResult<PyObject> {
        let db = self.db_for_storage(py)?;
        let sch = py_to_cypher_schema(schema)?;
        let params = convert_params(params)?;
        match py.allow_threads(|| db.run_cypher(query, &sch, params)) {
            Ok(rows) => Ok(named_rows_to_py(rows, py)),
            Err(err) => {
                Err(script_error(py, err, Some(query)))
            }
        }
    }
    pub fn register_callback(&self, rel: &str, callback: &PyAny) -> PyResult<u32> {
        let db = self.db_ref()?;
        let cb: Py<PyAny> = callback.into();
        let (id, ch) = db.register_callback(rel, None);
        rayon::spawn(move || {
            for (op, new, old) in ch {
                Python::with_gil(|py| {
                    let op = PyString::new(py, op.as_str()).into();
                    let new_py = rows_to_py_rows(new.rows, py);
                    let old_py = rows_to_py_rows(old.rows, py);
                    let args = PyTuple::new(py, [op, new_py, old_py]);
                    let callable = cb.as_ref(py);
                    if let Err(err) = callable.call1(args) {
                        eprintln!("{}", err);
                    }
                })
            }
        });
        Ok(id)
    }
    pub fn register_fixed_rule(
        &self,
        name: String,
        arity: usize,
        callback: &PyAny,
    ) -> PyResult<()> {
        let db = self.db_ref()?;
        let cb: Py<PyAny> = callback.into();
        let rule_impl = SimpleFixedRule::new(arity, move |inputs, options| -> Result<_> {
            Python::with_gil(|py| -> Result<NamedRows> {
                let py_inputs = PyList::new(
                    py,
                    inputs.into_iter().map(|nr| rows_to_py_rows(nr.rows, py)),
                );
                let py_opts = options_to_py(options, py).into_diagnostic()?;
                let args = PyTuple::new(py, vec![PyObject::from(py_inputs), py_opts]);
                let res = cb.as_ref(py).call1(args).into_diagnostic()?;
                Ok(NamedRows::new(vec![], py_to_rows(res).into_diagnostic()?))
            })
        });
        db.register_fixed_rule(name, rule_impl).map_err(report2py)
    }
    pub fn unregister_callback(&self, id: u32) -> bool {
        // Preserve the pre-fork behavior: a closed handle returns `false`,
        // not an error. Clone out of a momentary guard, drop it, then call.
        let maybe_db = self.db.read().unwrap().clone();
        match maybe_db {
            Some(db) => db.unregister_callback(id),
            None => false,
        }
    }
    pub fn unregister_fixed_rule(&self, name: &str) -> PyResult<bool> {
        // Preserve the pre-fork behavior: a closed handle returns `Ok(false)`,
        // not an error.
        let maybe_db = self.db.read().unwrap().clone();
        match maybe_db {
            Some(db) => match db.unregister_fixed_rule(name) {
                Ok(b) => Ok(b),
                Err(err) => Err(PyException::new_err(err.to_string())),
            },
            None => Ok(false),
        }
    }
    /// Set the ceiling, in bytes, on the total size of cached graph projections
    /// (mnestic fork; default 512 MiB).
    ///
    /// Enforced immediately: variants are evicted least-recently-used first
    /// until the cache fits. `0` evicts everything and turns caching off, while
    /// `::graph create`/`list`/`drop` keep working and every algorithm builds
    /// its adjacency fresh. Without this the ceiling was unreachable from
    /// Python, yet the engine's oversize-variant warning names it as the
    /// remedy.
    #[cfg(feature = "graph-algo")]
    pub fn set_graph_projection_capacity(&self, bytes: usize) -> PyResult<()> {
        let db = self.db_ref()?;
        db.set_graph_projection_capacity(bytes);
        Ok(())
    }
    pub fn export_relations(&self, py: Python<'_>, relations: Vec<String>) -> PyResult<PyObject> {
        let db = self.db_for_storage(py)?;
        let res = match py.allow_threads(|| db.export_relations(relations.iter())) {
            Ok(res) => res,
            Err(err) => return Err(PyException::new_err(err.to_string())),
        };
        let ret = PyDict::new(py);
        for (k, v) in res {
            ret.set_item(k, named_rows_to_py(v, py))?;
        }
        Ok(ret.into())
    }
    pub fn import_relations(&self, py: Python<'_>, data: &PyDict) -> PyResult<()> {
        let db = self.db_for_storage(py)?;
        let mut arg = BTreeMap::new();
        for (k, v) in data.iter() {
            let k = k.extract::<String>()?;
            let vals = py_to_named_rows(v)?;
            arg.insert(k, vals);
        }
        py.allow_threads(|| db.import_relations(arg))
            .map_err(report2py)
    }
    /// Atomically copy a local Parquet or Arrow IPC file into an existing
    /// stored relation (mnestic fork; feature `columnar-io`).
    #[cfg(feature = "columnar-io")]
    #[pyo3(signature = (
        relation,
        path,
        *,
        format,
        columns=None,
        batch_rows=8192,
        timeout=None,
        max_source_bytes=None,
        max_rows=None,
        max_decoded_batch_bytes=None,
        max_value_bytes=None,
        max_nesting_depth=16
    ))]
    #[allow(clippy::too_many_arguments)]
    pub fn import_columnar_file(
        &self,
        py: Python<'_>,
        relation: &str,
        path: &Bound<'_, PyAny>,
        format: &str,
        columns: Option<BTreeMap<String, String>>,
        batch_rows: usize,
        timeout: Option<f64>,
        max_source_bytes: Option<u64>,
        max_rows: Option<u64>,
        max_decoded_batch_bytes: Option<usize>,
        max_value_bytes: Option<usize>,
        max_nesting_depth: usize,
    ) -> PyResult<PyObject> {
        let format = match format {
            "parquet" => ColumnarFileFormat::Parquet,
            "arrow_ipc_file" => ColumnarFileFormat::ArrowIpcFile,
            "arrow_ipc_stream" => ColumnarFileFormat::ArrowIpcStream,
            other => {
                return Err(PyException::new_err(format!(
                    "unknown columnar format '{other}'; expected parquet, arrow_ipc_file, or arrow_ipc_stream"
                )))
            }
        };
        let timeout = match timeout {
            Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                Some(Duration::from_secs_f64(seconds))
            }
            Some(_) => {
                return Err(PyException::new_err(
                    "timeout must be a finite number greater than zero",
                ))
            }
            None => None,
        };
        let path: String = py
            .import_bound("os")?
            .getattr("fspath")?
            .call1((path,))?
            .extract()?;
        let options = ColumnarImportOptions::new(format)
            .with_columns(columns.unwrap_or_default())
            .with_batch_rows(batch_rows)
            .with_timeout(timeout)
            .with_max_source_bytes(max_source_bytes)
            .with_max_rows(max_rows)
            .with_max_decoded_batch_bytes(max_decoded_batch_bytes)
            .with_max_value_bytes(max_value_bytes)
            .with_max_nesting_depth(max_nesting_depth);
        let db = self.db_for_storage(py)?;
        let report = py
            .allow_threads(|| db.import_columnar_file(relation, path, &options))
            .map_err(report2py)?;
        let result = PyDict::new_bound(py);
        result.set_item("rows_processed", report.rows_processed)?;
        result.set_item("batches_processed", report.batches_processed)?;
        result.set_item(
            "search_indexes_requiring_rebuild",
            report.search_indexes_requiring_rebuild,
        )?;
        Ok(result.into_py(py))
    }
    pub fn backup(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        let db = self.db_for_storage(py)?;
        py.allow_threads(|| db.backup_db(path)).map_err(report2py)
    }
    pub fn restore(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        let db = self.db_for_storage(py)?;
        py.allow_threads(|| db.restore_backup(path))
            .map_err(report2py)
    }
    pub fn import_from_backup(
        &self,
        py: Python<'_>,
        in_file: &str,
        relations: Vec<String>,
    ) -> PyResult<()> {
        let db = self.db_for_storage(py)?;
        py.allow_threads(|| db.import_from_backup(in_file, &relations))
            .map_err(report2py)
    }
    /// Close the handle (mnestic fork, item D). Takes `&self` (via a write guard)
    /// so it no longer collides with an in-flight `run_script(&self)`'s shared
    /// PyCell borrow — the "Already borrowed" bug. Returns immediately even while
    /// queries run; the underlying storage drops when the last in-flight
    /// `DbInstance` clone is dropped.
    ///
    /// Open governed transactions are cancelled (0.19.3): their next call
    /// raises and nothing they wrote commits. A RocksDB path is released once
    /// their worker threads exit. Legacy `multi_transact` transactions are
    /// not affected.
    pub fn close(&self) -> bool {
        let had = self.db.write().unwrap().take().is_some();
        self.governed.cancel_all();
        had
    }
    pub fn multi_transact(&self, py: Python<'_>, write: bool) -> PyResult<CozoDbMulTx> {
        let db = self.db_for_storage(py)?;
        Ok(CozoDbMulTx {
            tx: db.multi_transaction(write),
        })
    }
    /// Open a governed transaction (0.19.3): one deadline for the whole
    /// transaction (`timeout` seconds from this call, required), at most
    /// `idle_timeout` seconds between statements, an optional per-statement
    /// estimated memory limit in bytes, and rollback on any error. Use as a
    /// context manager: a clean exit commits, an exception aborts.
    ///
    /// On SQLite a governed write excludes every other query on the same
    /// handle; such calls raise `transaction::governed_write_open` until it
    /// ends.
    #[pyo3(signature = (write, *, timeout, idle_timeout=None, mem_limit=None))]
    pub fn governed_transact(
        &self,
        py: Python<'_>,
        write: bool,
        timeout: &Bound<'_, PyAny>,
        idle_timeout: Option<&Bound<'_, PyAny>>,
        mem_limit: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<GovernedTx> {
        let total = governed::parse_seconds("timeout", timeout)?;
        let idle = match idle_timeout {
            Some(v) if !v.is_none() => governed::parse_seconds("idle_timeout", v)?,
            _ => std::time::Duration::from_secs(5),
        };
        let mem_limit = governed::parse_mem_limit(mem_limit)?;
        let deadline = governed::deadline_after("timeout", total)?;
        self.open_governed(py, write, deadline, idle, mem_limit)
    }
    /// Run `func(tx)` in a fresh governed transaction and commit it,
    /// retrying errors marked `retryable` with jittered exponential backoff
    /// (50 ms, doubling) until `max_attempts` or the overall `timeout`, which
    /// spans every attempt. Never retries an error with `maybe_committed`.
    /// The last error is raised unchanged. `func` must be safe to run more
    /// than once: effects outside the transaction repeat.
    #[pyo3(signature = (func, *, write=true, timeout=None, max_attempts=3, idle_timeout=None, mem_limit=None))]
    #[allow(clippy::too_many_arguments)]
    pub fn run_governed(
        &self,
        py: Python<'_>,
        func: &Bound<'_, PyAny>,
        write: bool,
        timeout: Option<&Bound<'_, PyAny>>,
        max_attempts: i64,
        idle_timeout: Option<&Bound<'_, PyAny>>,
        mem_limit: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyObject> {
        let total = match timeout {
            Some(v) if !v.is_none() => governed::parse_seconds("timeout", v)?,
            _ => std::time::Duration::from_secs(30),
        };
        let idle = match idle_timeout {
            Some(v) if !v.is_none() => governed::parse_seconds("idle_timeout", v)?,
            _ => std::time::Duration::from_secs(5),
        };
        let mem_limit = governed::parse_mem_limit(mem_limit)?;
        if !(1..=i64::from(u32::MAX)).contains(&max_attempts) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "max_attempts must be ≥ 1, got {max_attempts}"
            )));
        }
        let deadline = governed::deadline_after("timeout", total)?;
        governed::run_governed(
            py,
            &|py, deadline| self.open_governed(py, write, deadline, idle, mem_limit),
            func,
            deadline,
            max_attempts as u32,
        )
    }
    /// Test-only (feature `test-hooks`): make the next transaction commit on
    /// this database fail. Not part of the API.
    #[cfg(feature = "test-hooks")]
    pub fn _fail_next_commit_for_tests(&self) -> PyResult<()> {
        self.db_ref()?.fail_next_commit_for_tests();
        Ok(())
    }
}

#[pymethods]
impl CozoDbMulTx {
    pub fn abort(&self) -> PyResult<()> {
        self.tx
            .abort()
            .map_err(|err| PyException::new_err(err.to_string()))
    }
    pub fn commit(&self) -> PyResult<()> {
        self.tx
            .commit()
            .map_err(|err| PyException::new_err(err.to_string()))
    }
    pub fn run_script(&self, py: Python<'_>, query: &str, params: &PyDict) -> PyResult<PyObject> {
        let params = convert_params(params)?;
        match py.allow_threads(|| self.tx.run_script(query, params)) {
            Ok(rows) => Ok(named_rows_to_py(rows, py)),
            Err(err) => {
                Err(script_error(py, err, Some(query)))
            }
        }
    }
}

#[pyfunction]
fn eval_expressions(
    py: Python<'_>,
    query: &str,
    params: &PyDict,
    bindings: &PyDict,
) -> PyResult<PyObject> {
    let params = convert_params(params).unwrap();
    let bindings = convert_params(bindings).unwrap();
    match evaluate_expressions(query, &params, &bindings) {
        Ok(v) => Ok(value_to_py(v, py)),
        Err(err) => {
            Err(script_error(py, err, Some(query)))
        }
    }
}

#[pyfunction]
fn variables(py: Python<'_>, query: &str, params: &PyDict) -> PyResult<BTreeSet<String>> {
    let params = convert_params(params).unwrap();
    match get_variables(query, &params) {
        Ok(rows) => Ok(rows),
        Err(err) => {
            Err(script_error(py, err, Some(query)))
        }
    }
}

#[pymodule]
fn mnestic(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<CozoDbPy>()?;
    m.add_class::<CozoDbMulTx>()?;
    m.add_class::<GovernedTx>()?;
    let error_type = _py.get_type_bound::<MnesticError>();
    // Class-level defaults, so a user-constructed MnesticError (for example
    // in a test or a retry predicate) has every documented attribute.
    error_type.setattr("code", _py.None())?;
    error_type.setattr("kind", "other")?;
    error_type.setattr("retryable", false)?;
    error_type.setattr("maybe_committed", false)?;
    m.add("MnesticError", error_type)?;
    m.add_function(wrap_pyfunction!(eval_expressions, m)?)?;
    m.add_function(wrap_pyfunction!(variables, m)?)?;
    Ok(())
}
