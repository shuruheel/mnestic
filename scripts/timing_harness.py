#!/usr/bin/env python3
"""Development fixtures and resource supervisor for keyed_txtime_timing.

Standard library only. Never runs qualifying datasets or reports release gates.
"""
import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import random
import signal
import shutil
from collections import Counter
import statistics
import subprocess
import sys
import time


def digest(data):
    return hashlib.sha256(data).hexdigest()


def op(name, script='', *, rows=None, phases=(), **kwargs):
    value = dict(id=name, script=script, phases=list(phases), **kwargs)
    if rows is not None:
        value['expected_rows'] = rows
    return value


def plan(backend, kind):
    """Small independent fixtures, not a replacement for the reference generator."""
    rng = random.Random(99)
    setup, steps, verify = [], [], []
    result = dict(version=1, purpose='development', seed=99, backend=backend,
                  setup=setup, steps=steps, verify=verify)
    if kind == 'keyed':
        result['keyed_access'] = True
        setup.extend([op('node-schema', ':create node {uid:String => label:String,tombstone_reason:String?}'),
                      op('edge-schema', ':create edge {from:String,to:String}')])
        nodes = [[f'n{i:05}', 'label', 'deleted' if i % 2 == 0 else None] for i in range(500)]
        edges = [[f'e{i:05}', f'n{i % 520:05}'] for i in range(600)]
        rng.shuffle(nodes)
        setup.extend([op('nodes', '?[uid,label,tombstone_reason] <- $r :put node {uid=>label,tombstone_reason}', params={'r': nodes}),
                      op('edges', '?[from,to] <- $r :put edge {from,to}', params={'r': edges})])
        query = "?[f,t] := *edge{from:f,to:t}, *node{uid:t,tombstone_reason:'deleted'}"
        expected = [r for r in edges if int(r[1][1:]) < 500 and int(r[1][1:]) % 2 == 0]
        steps.append(dict(mode='serial', cell='K2', operations=[
            op('plan', '::explain {' + query + '}', save_rows=True),
            op('point', query, rows=expected, phases=['query_execution']),
            op('batch', batch=True, rows=expected, phases=['query_execution'])]))
        overhead = []
        for i in range(100):
            for enabled in ([True, False] if i % 2 == 0 else [False, True]):
                overhead.append(op(f'pair-{i}-{enabled}', '?[uid] := *node{uid}, uid=$u',
                                   params={'u': f'n{i:05}'}, rows=[[f'n{i:05}']], immutable=True,
                                   disabled=not enabled, phases=['query_execution'] if enabled else []))
        steps.append(dict(mode='serial', cell='overhead', operations=overhead))
        steps.append(dict(mode='serial', cell='errors', operations=[
            op('parse-error', 'not a query', expected_error='parser'),
            op('assert-error', "?[uid] := *node{uid}, uid='n00000' :assert none", expected_error='assert_none', phases=['query_execution'])]))
        return result

    temporal = kind != 'plain'
    schema = ':create chunk {cid:String' + (',tt:TxTime' if temporal else '') + ' => text:String,emb:<F32;64>,kind:String}'
    setup.append(op('schema', schema))
    vectors = []
    for _ in range(120):
        vec = [rng.uniform(-1, 1) for _ in range(64)]
        norm = math.sqrt(sum(v*v for v in vec))
        vectors.append([v/norm for v in vec])
    data = [[f'c{i:05}', f'token{i:05} common', vectors[i], 'note'] for i in range(120)]
    put = '?[cid,text,emb,kind] <- $r :put chunk {cid=>text,emb,kind}'
    setup.append(op('rows', put, params={'r': data}))
    fts = '::fts create chunk:fts {extractor:text,tokenizer:Simple,filters:[Lowercase]}'
    hnsw = '::hnsw create chunk:vec {dim:64,m:16,dtype:F32,fields:[emb],distance:Cosine,ef_construction:64}'
    if kind != 'build-fts':
        setup.append(op('fts', fts))
    if kind != 'build-hnsw':
        setup.append(op('hnsw', hnsw))
    commit_phases = ['commit'] + (['tt_commit_section'] if temporal else [])
    workers = []
    for w in range(4):
        schedule = []
        for cycle in range(2):
            for i in range(16):
                idx = w*20+i
                row = [f'c{idx:05}', f'changed{w} cycle{cycle}', vectors[(idx+cycle+1) % 120], 'note']
                schedule.append(op(f'w{w}-cycle{cycle}-row{i}', put, params={'r':[row]}, phases=commit_phases))
        workers.append(schedule)
    if kind in ['overhead-tt','overhead-contention']:
        width = 4 if kind == 'overhead-contention' else 1
        serial = []
        for pair in range(100):
            for trial,enabled in enumerate([True,False] if pair % 2 == 0 else [False,True]):
                operations = []
                for w in range(width):
                    idx = w*20 + pair % 16
                    payload = [f'c{idx:05}',f'overhead{pair} trial{trial}',vectors[(idx+trial+1)%120],'note']
                    operations.append(op(f'pair-{pair}-{enabled}',put,params={'r':[payload]},disabled=not enabled,
                                         phases=commit_phases if enabled else []))
                if width == 1:
                    serial.extend(operations)
                else:
                    steps.append(dict(mode='concurrent',cell=f'overhead_{pair}_{enabled}',workers=[[o] for o in operations]))
        if width == 1:
            steps.append(dict(mode='serial',cell='overhead',operations=serial))
    elif kind.startswith('build-'):
        assert backend == 'rocksdb', 'overlapping build fixture requires RocksDB'
        steps.append(dict(mode='build', cell=kind.replace('-', '_'),
                          ddl=op('ddl', fts if kind == 'build-fts' else hnsw,
                                 phases=['build_registration', 'build_backfill', 'build_catchup', 'build_publication', 'tt_ddl_section']),
                          workers=workers, interval_ms=10, max_attempts_per_writer=500, max_seconds=60))
    else:
        steps.append(dict(mode='serial', cell='search', operations=[
            op('fts', "?[cid] := ~chunk:fts{cid | query:'token00000',k:100} :order cid", rows=[['c00000']], phases=['query_execution']),
            op('vector', '?[cid] := ~chunk:vec{cid | query:vec($q),k:1,ef:100}', params={'q':vectors[0]}, rows=[['c00000']], phases=['query_execution'])]))
        if kind == 'concurrent':
            steps.append(dict(mode='concurrent', cell='writers', workers=workers))
        else:
            steps.append(dict(mode='serial', cell='updates', operations=workers[0]))
    verify.append(op('count', '?[count(cid)] := *chunk{cid}', rows=[[120]], phases=['query_execution']))
    verify.append(op('stale-term', "?[cid] := ~chunk:fts{cid | query:'token00000',k:100}", rows=[], phases=['query_execution']))
    if temporal:
        verify.append(op('historical-search-error', "?[cid] := ~chunk:fts{cid | query:'common',k:100} :as_of 0", expected_error='as_of_unsupported'))
        verify.append(op('historical-empty', '?[cid] := *chunk{cid} :as_of 0', rows=[], phases=['query_execution']))
    return result


def processes(root):
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss=,pcpu=,comm='], text=True)
    table = []
    for line in rows.splitlines():
        parts = line.split(None, 4)
        if len(parts) == 5:
            table.append((int(parts[0]), int(parts[1]), int(parts[2]), float(parts[3]), parts[4]))
    ids = {root}
    while True:
        extra = {pid for pid, ppid, *_ in table if ppid in ids}
        if extra <= ids:
            break
        ids |= extra
    return sum(rss for pid, _, rss, _, _ in table if pid in ids), table, ids


def source_snapshot(repo):
    repo = Path(repo).resolve()
    git = lambda *args: subprocess.check_output(['git', '-C', str(repo), *args])
    paths = git('ls-files', '-z', '--cached', '--others', '--exclude-standard').split(b'\0')
    hashes = {}
    for raw in paths:
        if not raw:
            continue
        name = os.fsdecode(raw)
        path = repo/name
        if path.is_file() and (name.startswith(('cozo-core/src/', 'cozo-core/examples/', 'cozorocks/src/', 'cozorocks/bridge/')) or name.endswith(('Cargo.toml', 'Cargo.lock', 'build.rs'))):
            hashes[name] = digest(path.read_bytes())
    return dict(worktree=str(repo), head=git('rev-parse','HEAD').decode().strip(), files=hashes,
                rocksdb_vendor=git('ls-tree','HEAD','cozorocks/rocksdb').decode().strip())


def build(source, target, output, jobs=2):
    if jobs not in [1, 2]:
        raise ValueError("native builds require one or two jobs")
    source, target, output = map(lambda p: Path(p).resolve(), (source,target,output))
    output.mkdir()
    before = source_snapshot(source)
    (output/'source-before.json').write_text(json.dumps(before,sort_keys=True,indent=2))
    env = os.environ.copy()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_PROFILE_RELEASE_DEBUG='0', CARGO_INCREMENTAL='0')
    if env.get('ROCKSDB_LIB_DIR') and not (Path(env['ROCKSDB_LIB_DIR'])/'librocksdb.a').is_file():
        raise ValueError('native RocksDB archive is missing; refusing a build with incomplete provenance')
    command = ['cargo','build','--locked','-p','mnestic','--features','storage-rocksdb,test-hooks',
               '--example','keyed_txtime_timing','--release','-j',str(jobs)]
    started = time.time()
    with (output/'build.log').open('x') as log:
        proc = subprocess.Popen(command,cwd=source,env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        try:
            code = proc.wait(timeout=7200)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid,signal.SIGTERM)
                try: proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid,signal.SIGKILL)
                    proc.wait()
    if code:
        raise RuntimeError(f'build failed: {code}; see {output}/build.log')
    after = source_snapshot(source)
    (output/'source-after.json').write_text(json.dumps(after,sort_keys=True,indent=2))
    if before != after:
        changes = [name for name in before['files'].keys() | after['files'].keys() if before['files'].get(name) != after['files'].get(name)]
        raise RuntimeError(f'source changed during build; artifact is not accepted: {changes}; compare saved source snapshots')
    binary = output/'keyed_txtime_timing'
    shutil.copy2(target/'release/examples/keyed_txtime_timing',binary)
    native = {}
    for key in ['ROCKSDB_LIB_DIR','ROCKSDB_INCLUDE_DIR','ROCKSDB_STATIC']:
        native[key] = env.get(key)
    if env.get('ROCKSDB_LIB_DIR'):
        native['librocksdb_sha256'] = digest((Path(env['ROCKSDB_LIB_DIR'])/'librocksdb.a').read_bytes())
    if env.get('ROCKSDB_INCLUDE_DIR'):
        root = Path(env['ROCKSDB_INCLUDE_DIR'])
        native['headers_sha256'] = digest(json.dumps({str(p.relative_to(root)):digest(p.read_bytes()) for p in sorted(root.rglob('*.h'))},sort_keys=True).encode())
    evidence = dict(source=before, executable_sha256=digest(binary.read_bytes()), executable=str(binary),
                    command=command, environment={k:env.get(k) for k in ['CARGO_TARGET_DIR','CARGO_PROFILE_RELEASE_DEBUG','CARGO_INCREMENTAL','RUSTFLAGS']},
                    native=native, rustc=subprocess.check_output(['rustc','-Vv'],text=True),
                    started_unix=started,finished_unix=time.time(),purpose='development')
    (output/'build.json').write_text(json.dumps(evidence,indent=2))
    return evidence


def supervise(binary, plan_path, output, build_manifest, timeout, max_rss, min_free, host_monitor=None):
    binary, plan_path, output = map(lambda p: Path(p).resolve(), (binary, plan_path, output))
    if output.exists():
        raise ValueError('output directory must not exist')
    build_bytes = Path(build_manifest).read_bytes()
    built = json.loads(build_bytes)
    if digest(binary.read_bytes()) != built['executable_sha256']:
        raise ValueError('binary does not match build manifest')
    provenance = output.with_suffix('.supervisor.json')
    resource_path = output.with_suffix('.resources.csv')
    stdout_path = output.with_suffix('.stdout.log')
    evidence = dict(purpose='development', build_manifest=built, build_manifest_sha256=digest(build_bytes),
                    binary_sha256=digest(binary.read_bytes()), plan_sha256=digest(plan_path.read_bytes()),
                    supervisor_sha256=digest(Path(__file__).read_bytes()),
                    environment={k: os.environ.get(k) for k in ['RAYON_NUM_THREADS','OMP_NUM_THREADS','OPENBLAS_NUM_THREADS']},
                    limits=dict(seconds=timeout, rss_mib=max_rss, min_free_mib=min_free),
                    started_unix=time.time(), status='running')
    # Open exclusively before launching; never overwrite evidence from an earlier run.
    with provenance.open('x') as f:
        json.dump(evidence, f, indent=2)
    started = time.monotonic()
    reason = None
    with resource_path.open('x') as resources, stdout_path.open('x') as log:
        writer = csv.writer(resources)
        writer.writerow(['elapsed_s','rss_kib','free_mib','other_busy_pids','swap_stat'])
        proc = subprocess.Popen([str(binary), str(plan_path), str(output)], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while proc.poll() is None:
                rss, table, ids = processes(proc.pid)
                stat = os.statvfs(output.parent)
                free = stat.f_bavail*stat.f_frsize/(1024**2)
                elapsed = time.monotonic()-started
                busy = [(pid, cpu, name) for pid, _, _, cpu, name in table if pid not in ids and cpu >= 100]
                swap = subprocess.check_output(['vm_stat'], text=True) if sys.platform == 'darwin' else Path('/proc/vmstat').read_text()
                writer.writerow([elapsed,rss,free,json.dumps(busy),json.dumps(swap)])
                resources.flush()
                if host_monitor is not None:
                    reason = host_monitor.observe(table, ids, swap)
                if elapsed > timeout: reason = 'wall_timeout'
                elif rss > max_rss*1024: reason = 'rss_limit'
                elif free < min_free: reason = 'disk_limit'
                if reason:
                    break
                time.sleep(1)
        except Exception as error:
            reason = f"supervisor_error: {error!r}"
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try: proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
    # Completion is a separate file; preserve the prelaunch provenance unchanged.
    outcome = dict(exit_code=proc.returncode, stop_reason=reason, elapsed_s=time.monotonic()-started,
                   performance_qualified=False)
    with output.with_suffix('.completion.json').open('x') as out:
        json.dump(outcome,out,indent=2)
    if reason or proc.returncode:
        raise RuntimeError(outcome)
    return outcome


def report(directory):
    directory = Path(directory)
    records, malformed = [], []
    for p in sorted(directory.glob('*.jsonl')):
        for n,line in enumerate(p.read_text().splitlines(),1):
            try:
                r = json.loads(line)
                if 'timing' in r:
                    records.append(r)
            except (ValueError,TypeError):
                malformed.append(f'{p.name}:{n}')
    plan_file, summary_file = directory/'plan.json', directory/'summary.json'
    completion_file = directory.with_suffix('.completion.json')
    if not all(p.exists() for p in [plan_file,summary_file,completion_file]):
        return dict(status='incomplete',reason='missing plan, summary or supervisor completion',
                    operations=len(records),malformed=malformed,performance_qualified=False)
    schedule = json.loads(plan_file.read_text())
    summary, completion = json.loads(summary_file.read_text()),json.loads(completion_file.read_text())
    expected = Counter()
    for cell,ops in [('setup',schedule.get('setup',[])),('verify',schedule.get('verify',[]))]:
        expected.update((cell,0,o['id']) for o in ops)
    build_cells = set()
    for step in schedule['steps']:
        cell = step['cell']
        if step['mode'] == 'serial':
            expected.update((cell,0,o['id']) for o in step['operations'])
        elif step['mode'] == 'concurrent':
            for writer,ops in enumerate(step['workers'],1):
                expected.update((cell,writer,o['id']) for o in ops)
        else:
            build_cells.add(cell)
            expected[(cell,0,step['ddl']['id'])] += 1
    observed = Counter((r['cell'],r['writer'],r['input_id']) for r in records
                       if r['cell'] not in build_cells or r['writer'] == 0)
    missing, unexpected = expected-observed, observed-expected
    ids = Counter(r['operation_id'] for r in records)
    duplicates = sum(n-1 for n in ids.values() if n > 1)
    failed = [r for r in records if not r.get('oracle_ok') or not r.get('trace_valid')]
    build_schedule_failures = []
    for step in schedule['steps']:
        if step['mode'] != 'build':
            continue
        cell = step['cell']
        ddl = [r for r in records if r['cell'] == cell and r['writer'] == 0 and r.get('timing')]
        ddl_end = ddl[0]['timing']['end_ns'] if len(ddl) == 1 else None
        for w,ops in enumerate(step['workers'],1):
            actual = sorted((r for r in records if r['cell'] == cell and r['writer'] == w),key=lambda r:r['operation_id'])
            valid = (2 <= len(actual) <= step['max_attempts_per_writer']
                     and all(r['input_id'] == ops[i % len(ops)]['id'] for i,r in enumerate(actual))
                     and ddl_end is not None and actual[-1].get('timing')
                     and actual[-1]['timing']['start_ns'] >= ddl_end)
            if not valid:
                build_schedule_failures.append(f'{cell}/{w}')
        if any(r['cell'] == cell and r['writer'] not in range(5) for r in records):
            build_schedule_failures.append(f'{cell}/unexpected-writer')
    complete = (not build_schedule_failures and summary.get('development_conformance') is True and completion.get('exit_code') == 0
                and not completion.get('stop_reason') and not missing and not unexpected
                and not duplicates and not failed and not malformed)
    overhead = [r for r in records if r['cell'] == 'overhead' or r['cell'].startswith('overhead_')]
    pairs = {}
    for r in overhead:
        pieces = r['input_id'].split('-')
        if len(pieces) == 3:
            pairs.setdefault((r['writer'],pieces[1]),{}).setdefault(pieces[2],[]).append(r)
    valid_pairs = []
    for (writer,_),pair in pairs.items():
        if set(pair) == {'True','False'} and all(len(v) == 1 for v in pair.values()):
            on,off = pair['True'][0],pair['False'][0]
            if all(r.get('oracle_ok') and r.get('trace_valid') and r.get('status') == 'ok' and isinstance(r.get('end_to_end_ns'),int) for r in [on,off]):
                valid_pairs.append((writer,on['end_to_end_ns'],off['end_to_end_ns']))
    by_writer = {}
    for w,on,off in valid_pairs:
        by_writer.setdefault(w,[]).append((on,off))
    overhead_valid = complete and len(valid_pairs) == len(pairs) and bool(by_writer) and all(len(v) == 100 for v in by_writer.values())
    phases = {}
    for record in records:
        if record['cell'] not in build_cells or not record.get('timing'):
            continue
        for phase in record['timing']['events']:
            if not phase['phase'].startswith('build_'):
                continue
            waits = [e for r in records if r['cell'] == record['cell'] and r.get('timing')
                     for e in r['timing']['events'] if e['phase'] == 'tt_commit_section']
            intervals = [(w['start_ns'],w['acquired_ns'] if w['acquired_ns'] is not None else w['end_ns']) for w in waits]
            count = sum(a < b and phase['start_ns'] < phase['end_ns'] and a < phase['end_ns'] and b > phase['start_ns'] for a,b in intervals)
            sections = [(max(s['acquired_ns'],phase['start_ns']), min(s['end_ns'],phase['end_ns']))
                        for s in record['timing']['events'] if s['phase']=='tt_ddl_section'
                        and s['acquired_ns'] is not None and s['acquired_ns'] < phase['end_ns'] and s['end_ns'] > phase['start_ns']]
            guaranteed = [(max(s['acquired_ns'],phase['start_ns']),min(s['release_begin_ns'],phase['end_ns']))
                          for s in record['timing']['events'] if s['phase']=='tt_ddl_section' and s['acquired_ns'] is not None
                          and s.get('release_begin_ns') is not None and s['acquired_ns'] < phase['end_ns'] and s['release_begin_ns'] > phase['start_ns']]
            critical = sum(a < b and any(start < end and a < end and b > start for start,end in guaranteed) for a,b in intervals)
            potential = sum(a < b and any(start < end and a < end and b > start for start,end in sections) for a,b in intervals)
            requires_section = phase['phase'] in ['build_registration','build_catchup','build_publication']
            key = f"{record['cell']}/{record['operation_id']}/{phase['phase']}"
            phases[key] = dict(writer_attempts_overlapping=count,
                               acquired_section_overlaps=critical if guaranteed else None,
                               potential_section_overlaps=potential if sections else None,
                               coverage_basis='acquired_tt_section' if requires_section else 'phase_envelope',
                               covered=(bool(guaranteed) and critical > 0) if requires_section else count > 0)
    return dict(status='complete' if complete else 'incomplete',development_only=True,operations=len(records),
                missing_operations=sum(missing.values()),unexpected_operations=sum(unexpected.values()),duplicate_operation_ids=duplicates,
                errors=sum(r['status'] != 'ok' for r in records),oracle_or_trace_failures=len(failed),malformed=malformed,build_schedule_failures=build_schedule_failures,
                overhead_pairs_observed=len(pairs),overhead_pairs_valid=len(valid_pairs),overhead_estimate_valid=overhead_valid,
                overhead_p50_ratio=(statistics.median(x for _,x,_ in valid_pairs)/statistics.median(y for _,_,y in valid_pairs) if overhead_valid else None),
                overhead_median_paired_delta_ns=(statistics.median(x-y for _,x,y in valid_pairs) if overhead_valid else None),
                overhead_by_writer={str(w):dict(pairs=len(v),p50_ratio=statistics.median(x for x,_ in v)/statistics.median(y for _,y in v)) for w,v in by_writer.items()} if overhead_valid else {},
                phase_coverage=phases,performance_qualified=False)


def main():
    parser = argparse.ArgumentParser(__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    gen = sub.add_parser('fixture')
    gen.add_argument('backend', choices=['mem','sqlite','rocksdb'])
    gen.add_argument('kind', choices=['keyed','plain','txtime','concurrent','overhead-tt','overhead-contention','build-fts','build-hnsw'])
    gen.add_argument('output', type=Path)
    run = sub.add_parser('run')
    run.add_argument('binary'); run.add_argument('plan'); run.add_argument('output')
    run.add_argument('--build-manifest',required=True)
    run.add_argument('--seconds', type=int, default=600)
    run.add_argument('--max-rss-mib', type=int, default=4096)
    run.add_argument('--min-free-mib', type=int, default=2048)
    comp = sub.add_parser('build')
    comp.add_argument('source'); comp.add_argument('target'); comp.add_argument('output')
    comp.add_argument('--jobs', type=int, choices=[1,2], default=2)
    rep = sub.add_parser('report'); rep.add_argument('directory')
    args = parser.parse_args()
    if args.command == 'fixture':
        with args.output.open('x') as out:
            json.dump(plan(args.backend,args.kind),out,sort_keys=True,indent=2)
    elif args.command == 'run':
        print(json.dumps(supervise(args.binary,args.plan,args.output,args.build_manifest,args.seconds,args.max_rss_mib,args.min_free_mib)))
    elif args.command == 'build':
        print(json.dumps(build(args.source,args.target,args.output,args.jobs),indent=2))
    else:
        print(json.dumps(report(args.directory),indent=2))


if __name__ == '__main__':
    main()
