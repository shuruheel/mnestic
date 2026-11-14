#!/usr/bin/env python3
"""Frozen matched-snapshot recording controls; development only."""
import argparse
from collections import Counter
import json
from pathlib import Path
import random
import csv
import gzip
import timing_harness as harness
from analyze_timing_diagnostics import quantile


def fixture(backend, temporal, kind):
    base = harness.plan(backend, 'txtime' if temporal else 'plain')
    width = 4 if kind == 'contention' else 1
    rows = base['setup'][1]['params']['r']
    warmup = [harness.op(f'warmup-{i}', '?[cid] := *chunk{cid}, cid=$id',
                        params={'id': rows[i][0]}, rows=[[rows[i][0]]], immutable=True,
                        phases=['query_execution']) for i in range(100)]
    rounds = []
    for pair in range(100):
        workers, verify = [], []
        for worker in range(width):
            row = list(rows[worker*20])
            if kind == 'read':
                operation = harness.op('read', '?[cid] := *chunk{cid}, cid=$id',
                                       params={'id': row[0]}, rows=[[row[0]]], immutable=True,
                                       phases=['query_execution'])
            else:
                row[1] = f'after{pair}writer{worker}'
                operation = harness.op('update', '?[cid,text,emb,kind] <- $r :put chunk {cid=>text,emb,kind}',
                                       params={'r': [row]}, phases=['commit']+(['tt_commit_section'] if temporal else []))
                verify.extend([
                    # Compare the vector in its declared F32 domain. JSON's
                    # decimal parsing can perturb an exactly widened F32 by
                    # one F64 ULP; that is not a stored-vector difference.
                    harness.op('current', '?[cid,text,kind,same_vector] := *chunk{cid,text,emb,kind}, cid=$id, same_vector = (emb == vec($expected_vector))',
                               params={'id':row[0],'expected_vector':row[2]},
                               rows=[[row[0],row[1],row[3],True]], immutable=True),
                    harness.op('fresh', '?[cid] := ~chunk:fts{cid | query:$q,k:100}',
                               params={'q':row[1]}, rows=[[row[0]]], immutable=True),
                    harness.op('stale', '?[cid] := ~chunk:fts{cid | query:$q,k:100}',
                               params={'q':rows[worker*20][1].split()[0]}, rows=[], immutable=True),
                    harness.op('vector', '?[cid] := ~chunk:vec{cid | query:vec($q),k:1,ef:100}',
                               params={'q':row[2]}, rows=[[row[0]]], immutable=True),
                ])
                if temporal:
                    verify.append(harness.op('history', '::history chunk [[$id]]', params={'id':row[0]},
                                             expected_row_count=2, immutable=True))
            workers.append(operation)
        verify.append(harness.op('count', '?[count(cid)] := *chunk{cid}', rows=[[120]], immutable=True))
        rounds.append(dict(workers=workers, verify=verify))
    return dict(version=1,purpose='development',seed=99,backend=backend,setup=base['setup'],
                steps=[],verify=[],overhead_control=dict(warmup=warmup,rounds=rounds))


def jsonl_records(directory, include_snapshots=False):
    for path in sorted(Path(directory).iterdir()):
        if not (path.name.endswith('.jsonl') or path.name.endswith('.jsonl.gz')):
            continue
        if not include_snapshots and path.name.startswith('snapshots.jsonl'):
            continue
        opener = gzip.open if path.name.endswith('.gz') else open
        with opener(path, 'rt') as source:
            for line in source:
                yield json.loads(line)


def report(directory, estimates=True):
    directory=Path(directory)
    snapshot_path=directory/'snapshots.jsonl'
    if not snapshot_path.exists(): snapshot_path=directory/'snapshots.jsonl.gz'
    required=[directory/'plan.json',directory/'summary.json',snapshot_path,directory.with_suffix('.completion.json'), directory/'manifest.json',directory.with_suffix('.supervisor.json'),directory.with_suffix('.resources.csv')]
    if not all(p.exists() for p in required):
        return dict(status='incomplete',reason='missing evidence',freeze_control_passed=False)
    plan=json.loads(required[0].read_text());control=plan['overhead_control']
    runner=json.loads(required[4].read_text());supervisor=json.loads(required[5].read_text())
    built=supervisor.get('build_manifest',{})
    executable=Path(built.get('executable','/nonexistent'))
    build_path=executable.with_name('build.json')
    plan_hash=harness.digest(required[0].read_bytes())
    with required[6].open() as resource_file:
        resources=list(csv.DictReader(resource_file))
    provenance_ok=(build_path.is_file() and executable.is_file()
        and harness.digest(build_path.read_bytes())==supervisor.get('build_manifest_sha256')
        and json.loads(build_path.read_text())==built
        and harness.digest(executable.read_bytes())==built.get('executable_sha256')
        and runner.get('executable_sha256')==supervisor.get('binary_sha256')==built.get('executable_sha256')
        and runner.get('plan_sha256')==supervisor.get('plan_sha256')==plan_hash)
    limits=supervisor.get('limits',{})
    resources_ok=(0 < limits.get('seconds',0) <= 180 and 0 < limits.get('rss_mib',0) <= 4096
        and limits.get('min_free_mib',0)>=2048 and bool(resources)
        and all(float(r['rss_kib'])<=4096*1024 and float(r['free_mib'])>=2048 for r in resources))
    summary=json.loads(required[1].read_text()); completion=json.loads(required[3].read_text())
    opener=gzip.open if snapshot_path.name.endswith('.gz') else open
    with opener(snapshot_path,'rt') as source:
        snapshots=[json.loads(line) for line in source]
    rows=[r for r in jsonl_records(directory) if 'timing' in r]
    width=len(control['rounds'][0]['workers'])
    expected=Counter(('setup',0,o['id']) for o in plan['setup'])
    for pair,round_ in enumerate(control['rounds']):
        for label in ['True','False']:
            expected.update(('overhead',i%width+1,f'warmup-{pair}-{label}-{i}') for i in range(len(control['warmup'])))
            expected.update(('overhead',w,f'pair-{pair}-{label}') for w in range(1,width+1))
            expected.update(('overhead_verify',0,f'verify-{pair}-{label}-{i}') for i in range(len(round_['verify'])))
    observed=Counter((r['cell'],r['writer'],r['input_id']) for r in rows)
    seeds=[r['files'] for r in snapshots if r.get('kind')=='seed']
    starts=[r for r in snapshots if r.get('kind')=='arm_start']
    ends=[r for r in snapshots if r.get('kind')=='arm_end']
    expected_arms=Counter((p,e) for p in range(100) for e in [True,False])
    snapshots_ok=(len(seeds)==1 and Counter((r['pair'],r['enabled']) for r in starts)==expected_arms
                  and Counter((r['pair'],r['enabled']) for r in ends)==expected_arms
                  and all(r['files']==seeds[0] for r in starts))
    valid=(observed==expected and len(control['rounds'])==100 and len(control['warmup'])==100
           and width in [1,4] and all(len(r['workers'])==width for r in control['rounds'])
           and snapshots_ok and provenance_ok and resources_ok and 0 < completion.get('elapsed_s',0) <= 180
           and summary.get('development_conformance') and completion.get('exit_code')==0
           and not completion.get('stop_reason') and len({r['operation_id'] for r in rows})==len(rows)
           and all(r['status']=='ok' and r['oracle_ok'] and r['trace_valid'] for r in rows))
    by_key={(r['cell'],r['writer'],r['input_id']):r for r in rows}
    if valid:
        for arm in ends:
            label=str(arm['enabled']);pair=arm['pair']
            measured=[by_key[('overhead',w,f'pair-{pair}-{label}')] for w in range(1,width+1)]
            warmups=arm.get('warmups',[])
            valid &= (set(w.get('worker') for w in warmups)==set(range(1,width+1)) and len(warmups)==width
                and all(w['start_ns']<=w['end_ns']<=min(r['timing']['start_ns'] for r in measured) for w in warmups)
                and all(isinstance(arm.get(k),int) and arm[k]>=0 for k in
                    ['copy_ns','validation_ns','open_ns','close_ns','final_inventory_ns','cleanup_ns']))
        for pair,round_ in enumerate(control['rounds']):
            for i,op in enumerate(round_['verify']):
                # Real commit timestamps differ, but history cardinality is checked by the runner.
                if op['script'].startswith('::history'): continue
                a=by_key[('overhead_verify',0,f'verify-{pair}-True-{i}')]
                b=by_key[('overhead_verify',0,f'verify-{pair}-False-{i}')]
                valid &= a['output_sha256']==b['output_sha256']
    result=dict(status='complete' if valid else 'incomplete',development_only=True,operations=len(rows),
                missing=sum((expected-observed).values()),unexpected=sum((observed-expected).values()),
                snapshots_match=snapshots_ok,provenance_valid=provenance_ok,resource_limits_valid=resources_ok,
                elapsed_s=completion.get('elapsed_s'),plan_sha256=plan_hash,executable_sha256=built.get('executable_sha256'),
                report_sha256=harness.digest(Path(__file__).read_bytes()),freeze_control_passed=False,performance_qualified=False)
    if not valid or not estimates: return result
    on=[]; off=[]
    worker_durations={w:{label:[] for label in ["True","False"]} for w in range(1,width+1)}
    rank_changes=0
    for pair in range(100):
        ranks={}
        for label,values in [('True',on),('False',off)]:
            group=[by_key[('overhead',w,f'pair-{pair}-{label}')] for w in range(1,width+1)]
            values.append(max(r['timing']['end_ns'] for r in group)-min(r['timing']['start_ns'] for r in group))
            for r in group:
                worker_durations[r['writer']][label].append(r['timing']['end_ns']-r['timing']['start_ns'])
            ranks[label]={r['writer']:i for i,r in enumerate(sorted(group,key=lambda r:(r['timing']['end_ns'],r['writer'])))}
        rank_changes+=sum(ranks['True'][w]!=ranks['False'][w] for w in range(1,width+1))
    estimates={}
    for q in [.5,.99]:
        upper={}
        for method in ['block10','conditional_iid']:
            rng=random.Random(20261114)
            samples=[]
            for _ in range(10000):
                if method=='block10':
                    starts=[rng.randrange(10)*10 for _ in range(10)]
                    indices=[i for start in starts for i in range(start,start+10)]
                else: indices=[rng.randrange(100) for _ in range(100)]
                samples.append(quantile([on[i] for i in indices],q)/quantile([off[i] for i in indices],q))
            upper[method]=quantile(samples,.95)
        estimates[str(q)]=dict(ratio=quantile(on,q)/quantile(off,q),upper95=upper,
                               conservative_upper95=max(upper.values()))
    result.update(per_writer={str(w):{label:{"p50_ns":quantile(v,.5),"p99_ns":quantile(v,.99)}
                                              for label,v in arms.items()} for w,arms in worker_durations.items()},
                  writer_finish_rank_changes=rank_changes,writer_pairs=100*width,
                  pairs=100,workers=width,statistic='complete round makespan',estimates=estimates,
                  freeze_control_passed=all(e['conservative_upper95']<=1.05 for e in estimates.values()),
                  limitation='Incremental recording cost over disabled hooks; not total instrumentation cost or release performance acceptance.')
    return result


def main():
    p=argparse.ArgumentParser(__doc__);sub=p.add_subparsers(dest='command',required=True)
    gen=sub.add_parser('fixture');gen.add_argument('backend',choices=['sqlite','rocksdb']);gen.add_argument('schema',choices=['plain','txtime']);gen.add_argument('kind',choices=['read','single','contention']);gen.add_argument('output',type=Path)
    rep=sub.add_parser('report');rep.add_argument('directory',type=Path)
    a=p.parse_args()
    if a.command=='fixture':
        with a.output.open('x') as out: json.dump(fixture(a.backend,a.schema=='txtime',a.kind),out,sort_keys=True,indent=2)
    else: print(json.dumps(report(a.directory),indent=2))


if __name__=='__main__': main()
