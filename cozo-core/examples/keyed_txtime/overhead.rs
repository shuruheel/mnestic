//! Matched-snapshot recording controls. All copying, opens, warm-up and checks
//! happen outside the measured calls; worker threads persist across all pairs.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Control {
    warmup: Vec<Operation>,
    rounds: Vec<Round>,
    #[serde(default)]
    shared_readonly: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Round {
    workers: Vec<Operation>,
    verify: Vec<Operation>,
}
struct Work {
    db: Arc<DbInstance>,
    operation: Operation,
    warmup: Vec<Operation>,
    ready: Arc<Barrier>,
    abort: Arc<AtomicBool>,
}

struct ScratchDirGuard(Option<PathBuf>);
impl Drop for ScratchDirGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn inventory(path: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        if path.is_dir() {
            for entry in fs::read_dir(path).into_diagnostic()? {
                walk(root, &entry.into_diagnostic()?.path(), out)?;
            }
        } else {
            ensure!(
                path.is_file() && !path.is_symlink(),
                "unexpected snapshot entry"
            );
            out.insert(
                path.strip_prefix(root)
                    .into_diagnostic()?
                    .to_string_lossy()
                    .into_owned(),
                hash(&fs::read(path).into_diagnostic()?),
            );
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    if path.is_dir() {
        walk(path, path, &mut out)?;
    } else {
        out.insert("database".into(), hash(&fs::read(path).into_diagnostic()?));
    }
    Ok(out)
}
fn copy_snapshot(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        fs::create_dir(target).into_diagnostic()?;
        for entry in fs::read_dir(source).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            copy_snapshot(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        ensure!(
            source.is_file() && !source.is_symlink() && !target.exists(),
            "invalid snapshot copy"
        );
        fs::copy(source, target).into_diagnostic()?;
    }
    Ok(())
}

fn logical_digest(db: &DbInstance) -> Result<String> {
    let rels = db
        .run_script("::relations", BTreeMap::new(), ScriptMutability::Immutable)?
        .into_json();
    let mut names: Vec<String> = rels["rows"]
        .as_array()
        .ok_or_else(|| miette!("expected ::relations rows array"))?
        .iter()
        .filter_map(|row| row.get(0).and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    names.sort();
    let exported: BTreeMap<String, Value> = db
        .export_relations(names.iter().map(String::as_str))?
        .into_iter()
        .map(|(k, v)| (k, v.into_json()))
        .collect();
    let payload =
        serde_json::to_vec(&json!({"relations": rels, "data": exported})).into_diagnostic()?;
    Ok(hash(&payload))
}

pub(super) fn run(plan: &Plan, dir: &Path, control: &Control) -> Result<()> {
    ensure!(
        ["sqlite", "rocksdb"].contains(&plan.backend.as_str()),
        "snapshot controls require a persistent backend"
    );
    ensure!(
        plan.steps.is_empty() && plan.verify.is_empty(),
        "controls own their measured/verification schedule"
    );
    ensure!(
        control.rounds.len() >= 100 && control.rounds.len() % 100 == 0,
        "require a positive multiple of 100 pairs per block"
    );
    ensure!(
        (20..=100).contains(&control.warmup.len()),
        "require 20..=100 warm-ups per arm"
    );
    let width = control.rounds[0].workers.len();
    ensure!(
        [1, 4].contains(&width) && control.rounds.iter().all(|r| r.workers.len() == width),
        "invalid worker width"
    );
    ensure!(
        control
            .warmup
            .iter()
            .all(|o| o.immutable && !o.batch && o.expected_error.is_none()),
        "warm-up must be read-only"
    );
    if control.shared_readonly {
        ensure!(width == 1, "shared_readonly requires single-worker read controls");
        ensure!(
            control.rounds.iter().all(|r| r
                .workers
                .iter()
                .all(|o| o.immutable && !o.batch && o.expected_error.is_none())
                && r.verify.iter().all(|o| o.immutable)),
            "shared_readonly requires all measured and verify ops to be immutable"
        );
    }
    let scratch_guard = match std::env::var_os("MNESTIC_OVERHEAD_SCRATCH_DIR") {
        Some(base) if !base.is_empty() => {
            let root = PathBuf::from(base);
            fs::create_dir_all(&root).into_diagnostic()?;
            let sub = root.join(format!("overhead-{}-{}", std::process::id(), now_ns()));
            fs::create_dir(&sub).into_diagnostic()?;
            ScratchDirGuard(Some(sub))
        }
        _ => ScratchDirGuard(None),
    };
    let arm_parent = scratch_guard.0.as_deref().unwrap_or(dir);
    let seed = dir.join("seed-database");
    let db = DbInstance::new(&plan.backend, seed.to_str().unwrap(), "{}")?;
    let timeout = Duration::from_secs(30);
    let mut setup = Context::new(dir, "setup", 0)?;
    for op in &plan.setup {
        setup.run(&db, op, timeout)?;
    }
    drop(db);
    if plan.backend == "sqlite" {
        let wal = PathBuf::from(format!("{}-wal", seed.display()));
        ensure!(
            !wal.exists() || fs::metadata(wal).into_diagnostic()?.len() == 0,
            "snapshot requires a fully checkpointed SQLite close"
        );
    }
    let seed_inventory = inventory(&seed)?;
    let mut provenance = file(&dir.join("snapshots.jsonl"))?;
    append(
        &mut provenance,
        &json!({"kind":"seed","files":seed_inventory,"shared_readonly":control.shared_readonly}),
    )?;
    let mut verify = Context::new(dir, "overhead_verify", 0)?;
    if control.shared_readonly {
        let path = arm_parent.join("shared-readonly-database");
        let copy_start = Instant::now();
        copy_snapshot(&seed, &path)?;
        let copy_ns = copy_start.elapsed().as_nanos();
        let validation_start = Instant::now();
        let initial = inventory(&path)?;
        let validation_ns = validation_start.elapsed().as_nanos();
        ensure!(
            initial == seed_inventory,
            "snapshot copy differs before opening"
        );
        let open_start = Instant::now();
        let db = DbInstance::new(&plan.backend, path.to_str().unwrap(), "{}")?;
        let open_ns = open_start.elapsed().as_nanos();
        let pre_logical_sha256 = logical_digest(&db)?;
        append(
            &mut provenance,
            &json!({"kind":"block_start","files":initial,"logical_sha256":pre_logical_sha256}),
        )?;
        if let Some(enabled) = plan.keyed_access {
            db.set_keyed_access_for_tests(enabled);
        }
        let mut context = Context::new(dir, "overhead", 1)?;
        let warmup_start = now_ns();
        for (i, op) in control.warmup.iter().enumerate() {
            let mut op = op.clone();
            op.id = format!("warmup-block-{i}");
            op.warmup = true;
            context.run(&db, &op, timeout)?;
        }
        let warmup_end = now_ns();
        let warmups = vec![json!({"worker":1,"start_ns":warmup_start,"end_ns":warmup_end})];
        for (pair, round) in control.rounds.iter().enumerate() {
            for enabled in if pair % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let label = if enabled { "True" } else { "False" };
                let mut operation = round.workers[0].clone();
                operation.id = format!("pair-{pair}-{label}");
                operation.disabled = !enabled;
                if !enabled {
                    operation.phases.clear();
                }
                context.run(&db, &operation, timeout)?;
            }
        }
        if let Some(last) = control.rounds.last() {
            for (i, op) in last.verify.iter().enumerate() {
                let mut op = op.clone();
                op.id = format!("verify-block-{i}");
                verify.run(&db, &op, timeout)?;
            }
        }
        let post_logical_sha256 = logical_digest(&db)?;
        ensure!(
            post_logical_sha256 == pre_logical_sha256,
            "shared_readonly logical database state changed during block"
        );
        let close_start = Instant::now();
        drop(db);
        let close_ns = close_start.elapsed().as_nanos();
        let inventory_start = Instant::now();
        let final_files = inventory(&path)?;
        let final_inventory_ns = inventory_start.elapsed().as_nanos();
        if plan.backend == "sqlite" {
            ensure!(
                final_files == seed_inventory,
                "shared_readonly database files changed during block"
            );
        }
        let cleanup_start = Instant::now();
        if path.is_dir() {
            fs::remove_dir_all(&path).into_diagnostic()?;
        } else {
            fs::remove_file(&path).into_diagnostic()?;
        }
        let cleanup_ns = cleanup_start.elapsed().as_nanos();
        append(
            &mut provenance,
            &json!({"kind":"block_end","files":final_files,"logical_sha256":post_logical_sha256,
            "copy_ns":copy_ns,"validation_ns":validation_ns,"open_ns":open_ns,"warmups":warmups,
            "close_ns":close_ns,"final_inventory_ns":final_inventory_ns,"cleanup_ns":cleanup_ns}),
        )?;
        return Ok(());
    }
    std::thread::scope(|scope| -> Result<()> {
        let contexts = (1..=width)
            .map(|w| Context::new(dir, "overhead", w))
            .collect::<Result<Vec<_>>>()?;
        let (completed, results) = mpsc::channel();
        let mut senders = Vec::new();
        let mut handles = Vec::new();
        for (worker, mut context) in contexts.into_iter().enumerate() {
            let (send, receive) = mpsc::channel::<Work>();
            senders.push(send);
            let completed = completed.clone();
            handles.push(scope.spawn(move || -> Result<()> {
                for work in receive {
                    let mut outcome = Ok(());
                    let warmup_start = now_ns();
                    for op in &work.warmup {
                        if let Err(error) = context.run(&work.db, op, timeout) {
                            work.abort.store(true, Ordering::Release);
                            outcome = Err(error);
                            break;
                        }
                    }
                    let warmup_end = now_ns();
                    work.ready.wait();
                    if !work.abort.load(Ordering::Acquire) {
                        outcome = context.run_synchronized(
                            &work.db,
                            &work.operation,
                            timeout,
                            Some(&work.ready),
                        );
                    } else if outcome.is_ok() {
                        outcome = Err(miette::miette!("another worker failed warm-up"));
                    }
                    drop(work); // drop this worker's database handle before acknowledgment
                    if completed
                        .send((worker, outcome, warmup_start, warmup_end))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(())
            }));
        }
        drop(completed);
        let outcome = (|| -> Result<()> {
            for (pair, round) in control.rounds.iter().enumerate() {
                for enabled in if pair % 2 == 0 {
                    [true, false]
                } else {
                    [false, true]
                } {
                    let label = if enabled { "True" } else { "False" };
                    let path = arm_parent.join(format!("pair-{pair}-{label}-database"));
                    let copy_start = Instant::now();
                    copy_snapshot(&seed, &path)?;
                    let copy_ns = copy_start.elapsed().as_nanos();
                    let validation_start = Instant::now();
                    let initial = inventory(&path)?;
                    let validation_ns = validation_start.elapsed().as_nanos();
                    ensure!(
                        initial == seed_inventory,
                        "snapshot copy differs before opening"
                    );
                    append(
                        &mut provenance,
                        &json!({"kind":"arm_start","pair":pair,"enabled":enabled,"files":initial}),
                    )?;
                    let open_start = Instant::now();
                    let db = Arc::new(DbInstance::new(
                        &plan.backend,
                        path.to_str().unwrap(),
                        "{}",
                    )?);
                    let open_ns = open_start.elapsed().as_nanos();
                    if let Some(enabled) = plan.keyed_access {
                        db.set_keyed_access_for_tests(enabled);
                    }
                    let ready = Arc::new(Barrier::new(width));
                    let abort = Arc::new(AtomicBool::new(false));
                    for (worker, send) in senders.iter().enumerate() {
                        let mut operation = round.workers[worker].clone();
                        operation.id = format!("pair-{pair}-{label}");
                        operation.disabled = !enabled;
                        if !enabled {
                            operation.phases.clear();
                        }
                        let warmup = control
                            .warmup
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| i % width == worker)
                            .map(|(i, op)| {
                                let mut op = op.clone();
                                op.id = format!("warmup-{pair}-{label}-{i}");
                                op.warmup = true;
                                op
                            })
                            .collect();
                        send.send(Work {
                            db: db.clone(),
                            operation,
                            warmup,
                            ready: ready.clone(),
                            abort: abort.clone(),
                        })
                        .into_diagnostic()?;
                    }
                    let mut errors = Vec::new();
                    let mut warmups = Vec::new();
                    for _ in 0..width {
                        let (worker, result, start_ns, end_ns) =
                            results.recv().into_diagnostic()?;
                        warmups
                            .push(json!({"worker":worker+1,"start_ns":start_ns,"end_ns":end_ns}));
                        if let Err(error) = result {
                            errors.push(format!("worker {worker}: {error:?}"));
                        }
                    }
                    ensure!(errors.is_empty(), "control failed: {}", errors.join("; "));
                    for (i, op) in round.verify.iter().enumerate() {
                        let mut op = op.clone();
                        op.id = format!("verify-{pair}-{label}-{i}");
                        verify.run(&db, &op, timeout)?;
                    }
                    let close_start = Instant::now();
                    drop(db);
                    let close_ns = close_start.elapsed().as_nanos();
                    let inventory_start = Instant::now();
                    let final_files = inventory(&path)?;
                    let final_inventory_ns = inventory_start.elapsed().as_nanos();
                    let cleanup_start = Instant::now();
                    if path.is_dir() {
                        fs::remove_dir_all(&path).into_diagnostic()?;
                    } else {
                        fs::remove_file(&path).into_diagnostic()?;
                    }
                    let cleanup_ns = cleanup_start.elapsed().as_nanos();
                    append(
                        &mut provenance,
                        &json!({"kind":"arm_end","pair":pair,"enabled":enabled,"files":final_files,
                        "copy_ns":copy_ns,"validation_ns":validation_ns,"open_ns":open_ns,"warmups":warmups,
                        "close_ns":close_ns,"final_inventory_ns":final_inventory_ns,"cleanup_ns":cleanup_ns}),
                    )?;
                }
            }
            Ok(())
        })();
        drop(senders);
        let joined = join_all(handles);
        outcome?;
        joined
    })
}
