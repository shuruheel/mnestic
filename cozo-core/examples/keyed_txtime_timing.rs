/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 */

//! Development/validation runner for the reviewed keyed/TxTime timing protocol.
//! Input is a frozen JSON operation schedule, output is append-only raw JSONL.
//! Qualification requires a frozen campaign bound to passing recording controls.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Barrier,
};
use std::time::{Duration, Instant};
#[path = "keyed_txtime/inputs.rs"]
mod inputs;
#[path = "keyed_txtime/overhead.rs"]
mod overhead;

static NEXT_CONTEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use cozo::{
    measurement::{capture, now_ns, t2_multi_get, Trace},
    DataValue, DbInstance, ScriptMutability, ScriptRunOptions,
};
use miette::{ensure, IntoDiagnostic, Result};
use serde_derive::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    purpose: String,
    seed: u64,
    backend: String,
    #[serde(default)]
    keyed_access: Option<bool>,
    #[serde(default)]
    overhead_control: Option<overhead::Control>,
    #[serde(default)]
    setup: Vec<Operation>,
    steps: Vec<Step>,
    #[serde(default)]
    verify: Vec<Operation>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    id: String,
    #[serde(default)]
    script: String,
    #[serde(default)]
    params: BTreeMap<String, Value>,
    #[serde(default)]
    params_file: Option<inputs::Input>,
    #[serde(default)]
    expected_rows_file: Option<inputs::Input>,
    #[serde(default)]
    params_from: BTreeMap<String, String>,
    #[serde(default)]
    capture: Option<CaptureValue>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    batch: bool,
    #[serde(default)]
    immutable: bool,
    #[serde(default)]
    warmup: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    expected_rows: Option<Value>,
    #[serde(default)]
    expected_row_count: Option<usize>,
    #[serde(default)]
    project_row_count: bool,
    #[serde(default)]
    save_rows: bool,
    #[serde(default)]
    expected_error: Option<String>,
    #[serde(default)]
    phases: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureValue {
    name: String,
    column: String,
    row: usize,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Serial {
        cell: String,
        operations: Vec<Operation>,
    },
    Concurrent {
        cell: String,
        workers: Vec<Vec<Operation>>,
    },
    Build {
        cell: String,
        ddl: Box<Operation>,
        workers: Vec<Vec<Operation>>,
        interval_ms: u64,
        max_attempts_per_writer: usize,
        max_seconds: u64,
        #[serde(default)]
        diagnostic_drain_after_ms: Option<u64>,
    },
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .into_diagnostic()?,
    ))
}
fn append(out: &mut BufWriter<File>, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value).into_diagnostic()?;
    out.write_all(b"\n").into_diagnostic()?;
    out.flush().into_diagnostic()
}

struct Context {
    cell: String,
    writer: usize,
    counter: u64,
    out: BufWriter<File>,
    bindings: BTreeMap<String, Value>,
}
impl Context {
    fn new(dir: &Path, cell: &str, writer: usize) -> Result<Self> {
        ensure!(
            !cell.is_empty()
                && cell
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "invalid cell name"
        );
        Ok(Self {
            bindings: BTreeMap::new(),
            cell: cell.into(),
            writer,
            counter: NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed) << 32,
            out: file(&dir.join(format!("{cell}-{writer}.jsonl")))?,
        })
    }
    fn run(&mut self, db: &DbInstance, op: &Operation, timeout: Duration) -> Result<()> {
        self.run_synchronized(db, op, timeout, None)
    }
    fn run_synchronized(
        &mut self,
        db: &DbInstance,
        op: &Operation,
        timeout: Duration,
        boundary: Option<&Barrier>,
    ) -> Result<()> {
        // Every worker reaches both barriers even when input preparation fails.
        let prepared = (|| -> Result<_> {
            // Decode parameters and initialize capture storage outside run_script timing.
            ensure!(
                op.expected_rows_file.is_none() || op.expected_rows.is_none(),
                "inline/file row oracle is exclusive"
            );
            let mut json_params: BTreeMap<String, Value> = match &op.params_file {
                Some(input) => serde_json::from_value(input.read()?).into_diagnostic()?,
                None => BTreeMap::new(),
            };
            for (key, value) in &op.params {
                ensure!(
                    json_params.insert(key.clone(), value.clone()).is_none(),
                    "inline parameter overwrites file input"
                );
            }
            for (param, binding) in &op.params_from {
                ensure!(
                    !json_params.contains_key(param),
                    "bound parameter overwrites literal input"
                );
                json_params.insert(
                    param.clone(),
                    self.bindings
                        .get(binding)
                        .ok_or_else(|| miette::miette!("missing captured parameter {binding}"))?
                        .clone(),
                );
            }
            let expected_rows = match &op.expected_rows_file {
                Some(input) => Some(input.read()?),
                None => op.expected_rows.clone(),
            };
            let timeout = match op.timeout_seconds {
                Some(seconds) => {
                    ensure!((1..=900).contains(&seconds), "invalid operation timeout");
                    Duration::from_secs(seconds).min(timeout)
                }
                None => timeout,
            };
            let params = json_params
                .iter()
                .map(|(k, v)| (k.clone(), DataValue::from(v.clone())))
                .collect::<BTreeMap<String, DataValue>>();
            Ok((params, json_params, expected_rows, timeout))
        })();
        let id = self.counter;
        self.counter += 1;
        let mutability = if op.immutable {
            ScriptMutability::Immutable
        } else {
            ScriptMutability::Mutable
        };
        if let Some(boundary) = boundary {
            boundary.wait();
        }
        let (params, json_params, expected_rows, timeout) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(boundary) = boundary {
                    boundary.wait();
                }
                append(
                    &mut self.out,
                    &json!({"cell":self.cell,"writer":self.writer,"input_id":op.id,
                    "operation_id":id,"status":"input_error","error":format!("{error:?}"),"timing":null}),
                )?;
                return Err(error);
            }
        };
        let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture(id, !op.disabled, || {
                if op.batch {
                    t2_multi_get(db, timeout)
                } else {
                    db.run_script_with_options(
                        &op.script,
                        params,
                        mutability,
                        ScriptRunOptions {
                            timeout: Some(timeout.as_secs_f64()),
                            ..Default::default()
                        },
                    )
                }
            })
        }));
        // Concurrent controls emit nothing until every measured capture ends,
        // including captures that unwind. Encoding/flush must not perturb peers.
        if let Some(boundary) = boundary {
            boundary.wait();
        }
        let (result, trace) = match execution {
            Ok(pair) => pair,
            Err(_) => {
                append(
                    &mut self.out,
                    &json!({"cell":self.cell,"writer":self.writer,"input_id":op.id,"operation_id":id,"status":"panic","timing":null}),
                )?;
                miette::bail!("engine panic in {}", op.id);
            }
        };
        let mut engine_row_count = None;
        let (rows, error, captured, capture_error) = match result {
            Ok(rows) => {
                engine_row_count = Some(rows.rows.len());
                // H1's physical-version count must not JSON-expand every vector.
                // The engine still constructs the full NamedRows inside capture.
                let data = if op.project_row_count {
                    json!({"headers":["count"],"rows":[[rows.rows.len()]]})
                } else {
                    rows.into_json()
                };
                let captured = op.capture.as_ref().and_then(|binding| {
                    let column = data["headers"]
                        .as_array()?
                        .iter()
                        .position(|h| h == &binding.column)?;
                    Some((
                        binding.name.clone(),
                        data["rows"].get(binding.row)?.get(column)?.clone(),
                    ))
                });
                let capture_error = if op.capture.is_some() && captured.is_none() {
                    Some("capture column or row missing")
                } else if captured
                    .as_ref()
                    .is_some_and(|(name, _)| self.bindings.contains_key(name))
                {
                    Some("capture name reused")
                } else {
                    None
                };
                (Some(data["rows"].clone()), None, captured, capture_error)
            }
            Err(e) => (None, Some(format!("{e:?}")), None, None),
        };
        let row_bytes = rows
            .as_ref()
            .map(|r| serde_json::to_vec(r).expect("JSON rows"));
        let oracle = if let Some(expected) = &op.expected_error {
            error.as_ref().is_some_and(|e| e.contains(expected))
        } else {
            error.is_none()
                && op.expected_row_count.is_none_or(|n| {
                    rows.as_ref()
                        .and_then(Value::as_array)
                        .is_some_and(|rows| rows.len() == n)
                })
                && expected_rows
                    .as_ref()
                    .is_none_or(|expected| rows.as_ref() == Some(expected))
        };
        let trace_valid = validate_trace(&trace)
            && op
                .phases
                .iter()
                .all(|p| trace.events.iter().any(|e| e.phase == p));
        let execution_ns: Option<u64> = (!op.disabled).then(|| {
            trace
                .events
                .iter()
                .filter(|e| e.phase == "query_execution")
                .map(|e| e.end_ns - e.start_ns)
                .sum()
        });
        // No event means no measured execution channel, not a synthetic zero.
        let execution_ns =
            execution_ns.filter(|_| trace.events.iter().any(|e| e.phase == "query_execution"));
        let waits: Vec<_> = trace
            .events
            .iter()
            .filter(|e| e.phase == "tt_commit_section")
            .collect();
        let wait_ns = waits
            .iter()
            .filter_map(|e| e.acquired_ns.map(|a| a - e.start_ns))
            .max();
        let hold_ns = waits
            .iter()
            .filter_map(|e| e.acquired_ns.map(|a| e.end_ns - a))
            .max();
        append(
            &mut self.out,
            &json!({
                "cell":self.cell,"writer":self.writer,"input_id":op.id,"operation_id":id,"warmup":op.warmup,
                "status":if error.is_some() {"error"} else {"ok"},"error":error,
                "oracle_ok":oracle,"trace_valid":trace_valid,"row_count":rows.as_ref().and_then(Value::as_array).map(Vec::len),
                "engine_row_count":engine_row_count,"project_row_count":op.project_row_count,
                "output_sha256":row_bytes.as_deref().map(hash),"expected_error":op.expected_error,
                "resolved_params_sha256":hash(&serde_json::to_vec(&json_params).into_diagnostic()?),"captured":captured,"capture_error":capture_error,
            "output_rows": if op.save_rows || !oracle || capture_error.is_some() { rows.as_ref() } else { None },
                "execution_ns":execution_ns,"end_to_end_ns":trace.end_ns-trace.start_ns,
                "wait_ns":wait_ns,"hold_ns":hold_ns,"timing":trace
            }),
        )?;
        ensure!(
            capture_error.is_none(),
            "operation {} failed capture: {:?}",
            op.id,
            capture_error
        );
        ensure!(oracle, "operation {} failed its result/error oracle", op.id);
        ensure!(
            trace_valid,
            "operation {} has invalid, missing or overflowing trace",
            op.id
        );
        if let Some((name, value)) = captured {
            ensure!(
                self.bindings.insert(name, value).is_none(),
                "capture name reused"
            );
        }
        Ok(())
    }
}
fn validate_trace(t: &Trace) -> bool {
    !t.overflow
        && t.start_ns <= t.end_ns
        && t.events.iter().all(|e| {
            t.start_ns <= e.start_ns
                && e.start_ns <= e.end_ns
                && e.end_ns <= t.end_ns
                && e.acquired_ns
                    .is_none_or(|a| e.start_ns <= a && a <= e.end_ns)
                && e.release_begin_ns
                    .is_none_or(|r| e.acquired_ns.is_some_and(|a| a <= r && r <= e.end_ns))
        })
}

fn run(plan: &Plan, dir: &Path) -> Result<()> {
    if let Some(control) = &plan.overhead_control {
        return overhead::run(plan, dir, control);
    }
    let db = DbInstance::new(&plan.backend, dir.join("database").to_str().unwrap(), "{}")?;
    if let Some(enabled) = plan.keyed_access {
        db.set_keyed_access_for_tests(enabled);
    }
    let timeout = Duration::from_secs(30);
    let mut setup = Context::new(dir, "setup", 0)?;
    for op in &plan.setup {
        let ddl = [
            "::fts create ",
            "::hnsw create ",
            "::lsh create ",
            "::reindex ",
        ]
        .iter()
        .any(|prefix| op.script.trim_start().starts_with(prefix));
        setup.run(
            &db,
            op,
            if ddl {
                Duration::from_secs(900)
            } else {
                timeout
            },
        )?;
    }
    let bindings = setup.bindings.clone();
    for step in &plan.steps {
        match step {
            Step::Serial { cell, operations } => {
                let mut context = Context::new(dir, cell, 0)?;
                context.bindings = bindings.clone();
                for op in operations {
                    context.run(&db, op, timeout)?;
                }
            }
            Step::Concurrent { cell, workers } => {
                ensure!(
                    !workers.is_empty() && workers.len() <= 4,
                    "require 1..4 workers"
                );
                let barrier = Arc::new(Barrier::new(workers.len()));
                let abort = Arc::new(AtomicBool::new(false));
                std::thread::scope(|scope| -> Result<()> {
                    // Open all outputs before spawning, so I/O failure cannot strand a barrier.
                    let contexts = (0..workers.len())
                        .map(|w| {
                            let mut context = Context::new(dir, cell, w + 1)?;
                            context.bindings = bindings.clone();
                            Ok(context)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let handles: Vec<_> = workers
                        .iter()
                        .zip(contexts)
                        .map(|(ops, mut context)| {
                            let barrier = barrier.clone();
                            let abort = abort.clone();
                            let db = &db;
                            scope.spawn(move || -> Result<()> {
                                barrier.wait();
                                for op in ops {
                                    if abort.load(Ordering::Acquire) {
                                        miette::bail!("another writer failed");
                                    }
                                    if let Err(e) = context.run(db, op, timeout) {
                                        abort.store(true, Ordering::Release);
                                        return Err(e);
                                    }
                                }
                                Ok(())
                            })
                        })
                        .collect();
                    join_all(handles)
                })?;
            }
            Step::Build {
                cell,
                ddl,
                workers,
                interval_ms,
                max_attempts_per_writer,
                max_seconds,
                diagnostic_drain_after_ms,
            } => {
                ensure!(
                    plan.backend == "rocksdb",
                    "build/writer overlap requires RocksDB"
                );
                ensure!(
                    workers.len() == 4 && workers.iter().all(|w| !w.is_empty()),
                    "require four nonempty writer schedules"
                );
                ensure!(
                    *interval_ms > 0
                        && *max_attempts_per_writer >= 2
                        && *max_attempts_per_writer <= 100_000
                        && *max_seconds > 0
                        && *max_seconds <= 900,
                    "invalid build budget"
                );
                ensure!(
                    diagnostic_drain_after_ms.is_none_or(|ms| ms > 0 && ms < *max_seconds * 1000),
                    "invalid diagnostic drain time"
                );
                let started = Instant::now();
                let done = AtomicBool::new(false);
                let abort = AtomicBool::new(false);
                let deadline = Instant::now() + Duration::from_secs(*max_seconds);
                let mut ddl_context = Context::new(dir, cell, 0)?;
                ddl_context.bindings = bindings.clone();
                std::thread::scope(|scope| -> Result<()> {
                    let contexts = (0..4)
                        .map(|w| {
                            let mut context = Context::new(dir, cell, w + 1)?;
                            context.bindings = bindings.clone();
                            Ok(context)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let (ready_tx, ready_rx) = mpsc::channel();
                    let handles: Vec<_> = workers.iter().zip(contexts).map(|(ops,mut context)| {
                        let ready = ready_tx.clone(); let db = &db; let done = &done; let abort = &abort;
                        scope.spawn(move || -> Result<()> {
                            let interval = Duration::from_millis(*interval_ms);
                            let mut scheduled = Instant::now();
                            let mut drained = false;
                            for i in 0..*max_attempts_per_writer {
                                if i > 0 && !drained && diagnostic_drain_after_ms.is_some_and(|ms| started.elapsed() >= Duration::from_millis(ms)) {
                                    drained = true;
                                    append(&mut context.out, &json!({"kind":"diagnostic_writer_drain","writer":context.writer,"at_ns":now_ns()}))?;
                                    while !done.load(Ordering::Acquire) && !abort.load(Ordering::Acquire) && Instant::now() < deadline {
                                        std::thread::sleep(Duration::from_millis(1));
                                    }
                                }
                                let final_commit = done.load(Ordering::Acquire);
                                if abort.load(Ordering::Acquire) || Instant::now() >= deadline { miette::bail!("build writer aborted/budget exhausted"); }
                                let result = context.run(db, &ops[i % ops.len()], timeout.min(deadline.saturating_duration_since(Instant::now())));
                                if i == 0 { let _ = ready.send(result.is_ok()); }
                                if let Err(e) = result { abort.store(true, Ordering::Release); return Err(e); }
                                if final_commit { return Ok(()); }
                                scheduled += interval;
                                let mut missed = 0;
                                while scheduled < Instant::now() { scheduled += interval; missed += 1; }
                                append(&mut context.out, &json!({"kind":"pacing","writer":context.writer,"after_operation":i,"missed_slots":missed,"at_ns":now_ns()}))?;
                                std::thread::sleep(scheduled.saturating_duration_since(Instant::now()));
                            }
                            abort.store(true, Ordering::Release);
                            Err(miette::miette!("build writer attempt budget exhausted"))
                        })
                    }).collect();
                    drop(ready_tx);
                    let outcome = (|| -> Result<()> {
                        for _ in 0..4 {
                            ensure!(
                                ready_rx.recv_timeout(timeout).into_diagnostic()?,
                                "writer failed before registration"
                            );
                        }
                        ensure!(!abort.load(Ordering::Acquire), "writer failed before DDL");
                        ddl_context.run(
                            &db,
                            ddl,
                            deadline.saturating_duration_since(Instant::now()),
                        )
                    })();
                    done.store(true, Ordering::Release);
                    if outcome.is_err() {
                        abort.store(true, Ordering::Release);
                    }
                    let writer_result = join_all(handles);
                    outcome?;
                    writer_result
                })?;
            }
        }
    }
    let mut verify = Context::new(dir, "verify", 0)?;
    verify.bindings = bindings;
    for op in &plan.verify {
        verify.run(&db, op, timeout)?;
    }
    Ok(())
}
fn join_all(handles: Vec<std::thread::ScopedJoinHandle<'_, Result<()>>>) -> Result<()> {
    let mut errors = Vec::new();
    for h in handles {
        match h.join() {
            Ok(Ok(())) => (),
            Ok(Err(e)) => errors.push(format!("{e:?}")),
            Err(_) => errors.push("writer panicked".into()),
        }
    }
    ensure!(errors.is_empty(), "writer failures: {}", errors.join("; "));
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        args.len() == 3,
        "usage: keyed_txtime_timing PLAN.json NEW_OUTPUT_DIRECTORY"
    );
    let bytes = fs::read(&args[1]).into_diagnostic()?;
    let plan: Plan = serde_json::from_slice(&bytes).into_diagnostic()?;
    let executable = std::env::current_exe().into_diagnostic()?;
    let executable_hash = hash(&fs::read(&executable).into_diagnostic()?);
    let authorization = inputs::authorize(&plan, &bytes, &executable_hash)?;
    ensure!(
        ["mem", "sqlite", "rocksdb"].contains(&plan.backend.as_str()),
        "unsupported backend"
    );
    let dir = PathBuf::from(&args[2]);
    fs::create_dir(&dir).into_diagnostic()?;
    fs::write(dir.join("plan.json"), &bytes).into_diagnostic()?;
    let mut previous = now_ns();
    let mut clock_step = u64::MAX;
    for _ in 0..1000 {
        let next = now_ns();
        if next > previous {
            clock_step = clock_step.min(next - previous);
        }
        previous = next;
    }
    let mut manifest = file(&dir.join("manifest.json"))?;
    append(
        &mut manifest,
        &json!({"version":1,"purpose":plan.purpose,"seed":plan.seed,"backend":plan.backend,
        "plan_sha256":hash(&bytes),"executable":executable,"executable_sha256":executable_hash,
        "authorization":authorization,
        "debug_assertions":cfg!(debug_assertions),"clock_min_observed_step_ns":if clock_step == u64::MAX { None } else { Some(clock_step) },"unix_start_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).into_diagnostic()?.as_secs()}),
    )?;
    let result = run(&plan, &dir);
    let mut summary = file(&dir.join("summary.json"))?;
    append(
        &mut summary,
        &json!({"development_conformance":plan.purpose == "development" && result.is_ok(),"execution_complete":result.is_ok(),"performance_qualified":false,"error":result.as_ref().err().map(|e|format!("{e:?}"))}),
    )?;
    result
}

#[cfg(test)]
mod runner_tests {
    use super::*;

    fn operation(value: Value) -> Operation {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn external_parameters_merge_without_overwrite_and_capture_is_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        let bytes = br#"{"value":42}"#;
        fs::write(&path, bytes).unwrap();
        let db = DbInstance::new("mem", "", "{}").unwrap();
        let mut context = Context::new(dir.path(), "input", 0).unwrap();
        let op = operation(
            json!({"id":"load","script":"?[id,value] <- [[$id,$value]]", "params":{"id":"a"},
            "params_file":{"path":path,"sha256":hash(bytes)},"expected_rows":[["a",42]],
            "capture":{"name":"saved","column":"value","row":0}}),
        );
        context.run(&db, &op, Duration::from_secs(1)).unwrap();
        let reuse = operation(
            json!({"id":"reuse","script":"?[value] <- [[$x]]", "params_from":{"x":"saved"},"expected_rows":[[42]]}),
        );
        context.run(&db, &reuse, Duration::from_secs(1)).unwrap();
        let duplicate = operation(
            json!({"id":"duplicate","script":"?[x] <- [[$value]]","params":{"value":7},
            "params_file":{"path":path,"sha256":hash(bytes)}}),
        );
        assert!(context
            .run(&db, &duplicate, Duration::from_secs(1))
            .is_err());
        fs::write(&path, br#"{"value":43}"#).unwrap();
        assert!(context.run(&db, &op, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn failed_capture_retains_trace_and_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new("mem", "", "{}").unwrap();
        let mut context = Context::new(dir.path(), "capture", 0).unwrap();
        let op = operation(
            json!({"id":"bad","script":"?[value] <- [[42]]", "capture":{"name":"x","column":"missing","row":0}}),
        );
        assert!(context.run(&db, &op, Duration::from_secs(1)).is_err());
        let row: Value = serde_json::from_str(
            fs::read_to_string(dir.path().join("capture-0.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["capture_error"], "capture column or row missing");
        assert_eq!(row["output_rows"], json!([[42]]));
        assert!(row["timing"].is_object());
        assert!(context.bindings.is_empty());
    }

    #[test]
    fn explicit_cardinality_projection_preserves_engine_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new("mem", "", "{}").unwrap();
        let mut context = Context::new(dir.path(), "count", 0).unwrap();
        let op = operation(json!({"id":"count","script":"?[x] <- [[1],[2],[3]]",
            "project_row_count":true,"save_rows":true,"expected_rows":[[3]]}));
        context.run(&db, &op, Duration::from_secs(1)).unwrap();
        let row: Value = serde_json::from_str(
            fs::read_to_string(dir.path().join("count-0.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["engine_row_count"], 3);
        assert_eq!(row["row_count"], 1);
        assert_eq!(row["output_rows"], json!([[3]]));
        assert!(row["project_row_count"].as_bool().unwrap());
    }

    #[test]
    fn one_invalid_input_does_not_strand_four_worker_barriers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DbInstance::new("mem", "", "{}").unwrap());
        let barrier = Arc::new(Barrier::new(4));
        let (send, recv) = mpsc::channel();
        let mut threads = Vec::new();
        for writer in 0..4 {
            let db = db.clone();
            let barrier = barrier.clone();
            let send = send.clone();
            let path = dir.path().to_path_buf();
            threads.push(std::thread::spawn(move || {
                let mut context = Context::new(&path, "barrier", writer).unwrap();
                let mut value = json!({"id":"read","script":"?[x] <- [[1]]","expected_rows":[[1]]});
                if writer == 0 {
                    value["params_file"] =
                        json!({"path":path.join("missing.json"),"sha256":"missing"});
                }
                let result = context.run_synchronized(
                    &db,
                    &operation(value),
                    Duration::from_secs(1),
                    Some(&barrier),
                );
                send.send((writer, result.is_ok())).unwrap();
            }));
        }
        drop(send);
        for _ in 0..4 {
            let (writer, ok) = recv
                .recv_timeout(Duration::from_secs(3))
                .expect("every worker exits its barriers");
            assert_eq!(ok, writer != 0);
        }
        for worker in threads {
            worker.join().unwrap();
        }
    }
}
