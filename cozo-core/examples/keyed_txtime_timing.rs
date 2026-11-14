/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 */

//! Development/validation runner for the reviewed keyed/TxTime timing protocol.
//! Input is a frozen JSON operation schedule, output is append-only raw JSONL.
//! Qualification is deliberately rejected until campaign manifests are reviewed.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Barrier,
};
use std::time::{Duration, Instant};
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
    save_rows: bool,
    #[serde(default)]
    expected_error: Option<String>,
    #[serde(default)]
    phases: Vec<String>,
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
        // Decode parameters and initialize capture storage outside run_script timing.
        let params = op
            .params
            .iter()
            .map(|(k, v)| (k.clone(), DataValue::from(v.clone())))
            .collect();
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
        let (rows, error) = match result {
            Ok(rows) => (Some(rows.into_json()["rows"].clone()), None),
            Err(e) => (None, Some(format!("{e:?}"))),
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
                && op
                    .expected_rows
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
                "output_sha256":row_bytes.as_deref().map(hash),"expected_error":op.expected_error,
            "output_rows": if op.save_rows { rows.as_ref() } else { None },
                "execution_ns":execution_ns,"end_to_end_ns":trace.end_ns-trace.start_ns,
                "wait_ns":wait_ns,"hold_ns":hold_ns,"timing":trace
            }),
        )?;
        ensure!(oracle, "operation {} failed its result/error oracle", op.id);
        ensure!(
            trace_valid,
            "operation {} has invalid, missing or overflowing trace",
            op.id
        );
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
        setup.run(&db, op, timeout)?;
    }
    for step in &plan.steps {
        match step {
            Step::Serial { cell, operations } => {
                let mut context = Context::new(dir, cell, 0)?;
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
                        .map(|w| Context::new(dir, cell, w + 1))
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
                std::thread::scope(|scope| -> Result<()> {
                    let contexts = (0..4)
                        .map(|w| Context::new(dir, cell, w + 1))
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
                            miette::bail!("build writer attempt budget exhausted")
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
    ensure!(plan.version == 1 && plan.purpose == "development" && plan.seed == 99, "only development seed 99 is enabled; qualifying execution needs a reviewed campaign manifest");
    ensure!(
        ["mem", "sqlite", "rocksdb"].contains(&plan.backend.as_str()),
        "unsupported backend"
    );
    let dir = PathBuf::from(&args[2]);
    fs::create_dir(&dir).into_diagnostic()?;
    fs::write(dir.join("plan.json"), &bytes).into_diagnostic()?;
    let executable = std::env::current_exe().into_diagnostic()?;
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
        &json!({"version":1,"purpose":"development","seed":99,"backend":plan.backend,
        "plan_sha256":hash(&bytes),"executable":executable,"executable_sha256":hash(&fs::read(&executable).into_diagnostic()?),
        "source_identity":"record build command and source patch in validation evidence; not a qualifying manifest",
        "debug_assertions":cfg!(debug_assertions),"clock_min_observed_step_ns":if clock_step == u64::MAX { None } else { Some(clock_step) },"unix_start_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).into_diagnostic()?.as_secs()}),
    )?;
    let result = run(&plan, &dir);
    let mut summary = file(&dir.join("summary.json"))?;
    append(
        &mut summary,
        &json!({"development_conformance":result.is_ok(),"performance_qualified":false,"error":result.as_ref().err().map(|e|format!("{e:?}"))}),
    )?;
    result
}
