#!/usr/bin/env python3
"""Phase-3 recording controls. A failure never enables qualification."""
import argparse
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shutil
import time

import benchmark_host as host
import overhead_controls as controls
import timing_harness as harness
from analyze_timing_diagnostics import quantile

BLOCKS = 10
PAIRS = 100
DRAWS = 10000
BUDGET = 7200


def save(path, value):
    with Path(path).open('x') as out:
        json.dump(value, out, indent=2, sort_keys=True)
        out.write('\n')


def file_hash(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as source:
        for chunk in iter(lambda: source.read(1024*1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def archive(directory):
    """Only remove the original after full decompressed identity is verified."""
    files = {}
    for path in sorted(Path(directory).glob('*.jsonl')):
        original_hash = file_hash(path)
        target = path.with_name(path.name+'.gz')
        with target.open('xb') as raw:
            with gzip.GzipFile(fileobj=raw, mode='wb', mtime=0) as out, path.open('rb') as source:
                shutil.copyfileobj(source, out)
        digest = hashlib.sha256()
        with gzip.open(target, 'rb') as source:
            for chunk in iter(lambda: source.read(1024*1024), b''):
                digest.update(chunk)
        if digest.hexdigest() != original_hash:
            raise ValueError('archive differs from original: '+str(path))
        files[path.name] = dict(bytes=path.stat().st_size, sha256=original_hash,
                                archive=target.name, archive_sha256=file_hash(target))
    # Record recoverability before the first removal.
    save(Path(directory)/'archive.json', files)
    for name in files:
        (Path(directory)/name).unlink()
    return files


def measured_pairs(directory):
    rows = {}
    for row in controls.jsonl_records(directory):
        if row.get('cell') == 'overhead' and row.get('input_id','').startswith('pair-'):
            _, pair, enabled = row['input_id'].split('-')
            key = (int(pair), enabled, row['writer'])
            if key in rows:
                raise ValueError('duplicate measured row')
            if not all(row.get(k) for k in ['oracle_ok','trace_valid']) or row['status'] != 'ok':
                raise ValueError('invalid measured row')
            rows[key] = row
    widths = {r['writer'] for r in rows.values()}
    width = len(widths)
    expected = {(p,e,w) for p in range(PAIRS) for e in ['True','False'] for w in range(1,width+1)}
    if width not in [1,4] or set(rows) != expected:
        raise ValueError('incomplete paired schedule')
    values = {e:[] for e in ['True','False']}
    for pair in range(PAIRS):
        for enabled in values:
            group = [rows[pair,enabled,w]['timing'] for w in range(1,width+1)]
            values[enabled].append(max(r['end_ns'] for r in group)-min(r['start_ns'] for r in group))
    if any(v <= 0 for arm in values.values() for v in arm):
        raise ValueError('nonpositive measured duration')
    return values['True'], values['False']


def estimates(blocks, draws=DRAWS, check_budget=lambda: None):
    if len(blocks) != BLOCKS or any(len(a) != PAIRS or len(b) != PAIRS for a,b in blocks):
        raise ValueError('all ten complete blocks are required')
    if any(v <= 0 for arms in blocks for arm in arms for v in arm):
        raise ValueError('nonpositive duration')
    on = [v for a,_ in blocks for v in a]
    off = [v for _,b in blocks for v in b]
    output = {str(q):dict(ratio=quantile(on,q)/quantile(off,q),upper95={}) for q in [.5,.99]}
    for method in ['hierarchical_block10','conditional_iid']:
        rng = random.Random(20261114)
        distributions = {.5:[],.99:[]}
        for draw in range(draws):
            if draw % 100 == 0:
                check_budget()
            if method == 'hierarchical_block10':
                selected = [rng.randrange(BLOCKS) for _ in range(BLOCKS)]
                indices = []
                for block in selected:
                    starts = [rng.randrange(10)*10 for _ in range(10)]
                    indices.extend(block*PAIRS+i for start in starts for i in range(start,start+10))
            else:
                indices = [rng.randrange(BLOCKS*PAIRS) for _ in range(BLOCKS*PAIRS)]
            a = sorted(on[i] for i in indices)
            b = sorted(off[i] for i in indices)
            for q in distributions:
                k = math.ceil(q*len(a))-1
                distributions[q].append(a[k]/b[k])
        for q,samples in distributions.items():
            output[str(q)]['upper95'][method] = quantile(samples,.95)
    for e in output.values():
        e['conservative_upper95'] = max(e['upper95'].values())
    return dict(estimates=output,blocks=BLOCKS,pairs=BLOCKS*PAIRS,
                block_quantiles=[{str(q):dict(on_ns=quantile(a,q),off_ns=quantile(b,q)) for q in [.5,.99]} for a,b in blocks],
                freeze_control_passed=all(e['conservative_upper95'] <= 1.05 for e in output.values()))


def validate_frozen_builds(builds):
    for arm, item in builds.items():
        path=Path(item['directory'])
        if file_hash(path/'build.json') != item['build_manifest_sha256']:
            raise ValueError(arm+' build manifest changed after freeze')
        if file_hash(path/'keyed_txtime_timing') != item['executable_sha256']:
            raise ValueError(arm+' executable changed after freeze')


def semantic_environment(build):
    return {key:value for key,value in build['environment'].items() if key != 'CARGO_TARGET_DIR'}


def validate_review(builds, review):
    if review.get('phase') != 3 or review.get('implementation_review_passed') is not True:
        raise ValueError('phase 3 implementation review required')
    for arm,item in builds.items():
        expected=review['builds'][arm]
        if expected['build_manifest_sha256'] != item['build_manifest_sha256'] or expected['executable_sha256'] != item['executable_sha256']:
            raise ValueError('build differs from reviewed artifact: '+arm)
        if expected['source'] != item['build']['source']:
            raise ValueError('source differs from reviewed artifact: '+arm)
    for name in ['cozo-core/src/measurement.rs','cozo-core/examples/keyed_txtime/inputs.rs']:
        if builds['candidate']['build']['source']['files'].get(name) != builds['baseline']['build']['source']['files'].get(name):
            raise ValueError('shared measurement/input source differs: '+name)
    # The reviewed baseline export changes only the two absent optimizer-toggle
    # calls in runner/helper; both exact hashes must match the accepted patch.
    for name in ['cozo-core/examples/keyed_txtime_timing.rs','cozo-core/examples/keyed_txtime/overhead.rs']:
        for arm,item in builds.items():
            if review['shared_runner'][name][arm] != item['build']['source']['files'][name]:
                raise ValueError('shared runner differs from reviewed export')
    if builds['baseline']['build']['source']['head'] != '272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea':
        raise ValueError('baseline differs from protocol')


def run(candidate, baseline, output, review_manifest):
    candidate,baseline,output = (Path(p).resolve() for p in [candidate,baseline,output])
    output.mkdir()
    started = time.monotonic()
    environment = dict(RAYON_NUM_THREADS='2',OMP_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1')
    os.environ.update(environment)
    scripts = [Path(__file__),Path(host.__file__),Path(controls.__file__),Path(harness.__file__),
               Path(__file__).with_name('analyze_timing_diagnostics.py')]
    protocol = Path(__file__).resolve().parents[1]/'docs/plans/keyed-txtime-controls-phase3.md'
    builds = {}
    for arm,path in [('candidate',candidate),('baseline',baseline)]:
        info = json.loads((path/'build.json').read_text())
        if file_hash(path/'keyed_txtime_timing') != info['executable_sha256']:
            raise ValueError('unfrozen executable')
        # Old development-only runner binaries cannot qualify this new phase.
        source_files = info['source']['files']
        if 'cozo-core/examples/keyed_txtime/inputs.rs' not in source_files:
            raise ValueError('qualification-capable runner required before phase 3')
        builds[arm] = dict(directory=str(path),executable_sha256=info['executable_sha256'],
                           build_manifest_sha256=file_hash(path/'build.json'),build=info)
    for key in ['native','rustc']:
        if builds['candidate']['build'][key] != builds['baseline']['build'][key]:
            raise ValueError('mismatched build resources: '+key)
    if semantic_environment(builds['candidate']['build']) != semantic_environment(builds['baseline']['build']):
        raise ValueError('mismatched compiler/profile flags')
    review_manifest=Path(review_manifest).resolve()
    review=json.loads(review_manifest.read_text())
    validate_review(builds,review)
    cells = []
    for backend in ['sqlite','rocksdb']:
        for arm,temporal in [('baseline',False),('candidate',False),('candidate',True)]:
            for kind in ['read','single','contention']:
                name = f'{arm}-{backend}-{"txtime" if temporal else "plain"}-{kind}'
                path = output/(name+'.json')
                save(path,controls.fixture(backend,temporal,kind))
                cells.append(dict(name=name,arm=arm,plan_sha256=file_hash(path)))
    frozen = dict(version=1,purpose='development',phase=3,builds=builds,cells=cells,
                  review_manifest=str(review_manifest),review_manifest_sha256=file_hash(review_manifest),
                  order=[dict(block=b,cell=c['name']) for b in range(BLOCKS) for c in cells],
                  script_sha256={str(p.resolve()):file_hash(p) for p in scripts},
                  protocol=str(protocol),protocol_sha256=file_hash(protocol),environment=environment,
                  blocks=BLOCKS,pairs_per_block=PAIRS,draws=DRAWS,total_budget_seconds=BUDGET,
                  host=dict(idle_seconds=30,max_preflight_seconds=300,other_cpu_percent=25),
                  cell_seconds=180,rss_mib=4096,min_free_mib=2048)
    save(output/'campaign-manifest.json',frozen)
    observations = {c['name']:[] for c in cells}
    aggregate = {}
    stop = None
    completed = 0
    def budget():
        if time.monotonic()-started >= BUDGET:
            raise RuntimeError('campaign wall budget')
    try:
        for block in range(BLOCKS):
            for cell in cells:
                budget()
                validate_frozen_builds(builds)
                if file_hash(review_manifest) != frozen['review_manifest_sha256']:
                    raise ValueError('review manifest changed after freeze')
                name = cell['name']; prefix = output/f'{name}-block-{block:02}'
                pre = host.preflight(prefix.with_suffix('.preflight.jsonl'),output,
                                     max_seconds=min(300,BUDGET-(time.monotonic()-started)))
                save(prefix.with_suffix('.preflight-result.json'),pre)
                if not pre['eligible']:
                    stop = dict(reason='host_preflight',block=block,cell=name,detail=pre)
                    raise RuntimeError('host preflight did not pass')
                validate_frozen_builds(builds)
                build = Path(builds[cell['arm']]['directory'])
                monitor = host.Monitor(prefix.with_suffix('.host.jsonl'),output,pre['final_sample']['swapouts'])
                failure = None
                try:
                    harness.supervise(build/'keyed_txtime_timing',output/(name+'.json'),prefix,
                                      build/'build.json',min(180,BUDGET-(time.monotonic()-started)),4096,2048,monitor)
                except RuntimeError as error:
                    failure = str(error)
                finally:
                    monitor.close()
                result = controls.report(prefix,estimates=False)
                result.update(block=block,supervisor_error=failure)
                if result.get('plan_sha256') != cell['plan_sha256'] or result.get('executable_sha256') != builds[cell['arm']]['executable_sha256']:
                    result['status'] = 'incomplete'
                save(prefix.with_suffix('.report.json'),result)
                if result['status'] != 'complete' or failure:
                    stop = dict(reason='invalid_block',block=block,cell=name)
                    raise RuntimeError('control block failed; no replacement')
                observations[name].append(measured_pairs(prefix))
                archive(prefix)
                completed += 1
                print(json.dumps(dict(block=block,cell=name,status='complete',completed_blocks=completed)),flush=True)
        validate_frozen_builds(builds)
        for cell in cells:
            budget()
            result = estimates(observations[cell['name']],check_budget=budget)
            aggregate[cell['name']] = result
            save(output/(cell['name']+'-aggregate.json'),result)
    except (RuntimeError,ValueError,KeyError,TypeError,OSError) as error:
        stop = stop or dict(reason='campaign_error',error=str(error))
    try:
        validate_frozen_builds(builds)
    except (ValueError,OSError) as error:
        stop=stop or dict(reason='build_provenance_changed',error=str(error))
    unchanged = file_hash(review_manifest) == frozen['review_manifest_sha256'] and all(file_hash(p) == digest for p,digest in frozen['script_sha256'].items()) and file_hash(protocol) == frozen['protocol_sha256']
    elapsed = time.monotonic()-started
    passed = (stop is None and elapsed <= BUDGET and unchanged and completed == 180 and len(aggregate) == 18
              and all(r['freeze_control_passed'] for r in aggregate.values()))
    result = dict(version=1,phase=3,status='complete' if completed == 180 and len(aggregate) == 18 and not stop else 'incomplete',
                  elapsed_s=elapsed,stop_reason=stop,completed_blocks=completed,required_blocks=180,
                  scripts_and_protocol_unchanged=unchanged,all_recording_controls_passed=passed,
                  performance_qualified=False,campaign_manifest_sha256=file_hash(output/'campaign-manifest.json'))
    save(output/'campaign-completion.json',result)
    print(json.dumps(result,indent=2),flush=True)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('candidate_build'); parser.add_argument('baseline_build'); parser.add_argument('new_output')
    parser.add_argument('--review-manifest',required=True)
    args = parser.parse_args()
    run(args.candidate_build,args.baseline_build,args.new_output,args.review_manifest)
