#!/usr/bin/env python3
"""Run all 18 reviewed development controls once with a frozen manifest."""
import argparse
import json
import os
from pathlib import Path
import time
import overhead_controls as controls
import timing_harness as harness


def run(candidate, baseline, output):
    candidate,baseline,output=(Path(p).resolve() for p in (candidate,baseline,output))
    output.mkdir()
    environment=dict(RAYON_NUM_THREADS='2',OMP_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1')
    os.environ.update(environment)
    protocol=Path(__file__).resolve().parents[1]/'docs/plans/keyed-txtime-measurement-amendment.md'
    source_paths=[Path(__file__),Path(controls.__file__),Path(harness.__file__),Path(__file__).with_name("analyze_timing_diagnostics.py")]
    source_hashes={p.name:harness.digest(p.read_bytes()) for p in source_paths}
    builds={}
    for arm,path in [('candidate',candidate),('baseline',baseline)]:
        manifest=json.loads((path/'build.json').read_text())
        binary=path/'keyed_txtime_timing'
        if harness.digest(binary.read_bytes())!=manifest['executable_sha256']:
            raise ValueError(f'{arm} binary differs from build manifest')
        builds[arm]=dict(directory=str(path),manifest_sha256=harness.digest((path/'build.json').read_bytes()),
                         executable_sha256=manifest['executable_sha256'])
    cells=[]
    for backend in ['sqlite','rocksdb']:
        for arm,temporal in [('baseline',False),('candidate',False),('candidate',True)]:
            for kind in ['read','single','contention']:
                name=f'{arm}-{backend}-{"txtime" if temporal else "plain"}-{kind}'
                plan=output/(name+'.json')
                with plan.open('x') as out: json.dump(controls.fixture(backend,temporal,kind),out,indent=2,sort_keys=True)
                cells.append(dict(name=name,arm=arm,plan_sha256=harness.digest(plan.read_bytes())))
    frozen=dict(purpose='development',performance_qualified=False,script_sha256=source_hashes,builds=builds,
                cells=cells,total_budget_seconds=1800,cell_budget_seconds=180,rss_mib=4096,min_free_mib=2048,
                protocol=str(protocol),protocol_sha256=harness.digest(protocol.read_bytes()),environment=environment)
    (output/'campaign-manifest.json').write_text(json.dumps(frozen,indent=2)+'\n')
    started=time.monotonic();reports={};stop=None
    for cell in cells:
        remaining=1800-(time.monotonic()-started)
        free=os.statvfs(output);free_mib=free.f_bavail*free.f_frsize/(1024**2)
        _,table,_=harness.processes(os.getpid())
        competing=[name for _,_,_,_,name in table if Path(name).name in ['cargo','rustc','clang','clang++','clippy-driver']]
        if remaining<=0 or free_mib<2048 or competing:
            stop=dict(reason='prelaunch_limit',remaining_seconds=remaining,free_mib=free_mib,competing=competing);break
        name=cell['name'];build=Path(builds[cell['arm']]['directory']);directory=output/(name+'-run')
        try:
            harness.supervise(build/'keyed_txtime_timing',output/(name+'.json'),directory,build/'build.json',min(180,remaining),4096,2048)
        except RuntimeError as error:
            (output/(name+'-error.json')).write_text(json.dumps({'error':str(error)},indent=2))
        try:
            result=controls.report(directory)
        except (ValueError, KeyError, OSError, TypeError) as error:
            result=dict(status='incomplete',freeze_control_passed=False,error=str(error))
        result['frozen_plan_matches']=result.get('plan_sha256')==cell['plan_sha256']
        result['frozen_binary_matches']=result.get('executable_sha256')==builds[cell['arm']]['executable_sha256']
        if not result['frozen_plan_matches'] or not result['frozen_binary_matches']:
            result['freeze_control_passed']=False
        reports[name]=result
        (output/(name+'-report.json')).write_text(json.dumps(result,indent=2)+'\n')
        completion_path=directory.with_suffix('.completion.json')
        completion=(json.loads(completion_path.read_text()) if completion_path.exists()
                    else dict(stop_reason='missing supervisor completion'))
        if completion.get('stop_reason'):
            stop=completion;break
    elapsed=time.monotonic()-started
    scripts_unchanged=source_hashes=={p.name:harness.digest(p.read_bytes()) for p in source_paths}
    protocol_unchanged=harness.digest(protocol.read_bytes())==frozen['protocol_sha256']
    passed=(protocol_unchanged and len(reports)==18 and elapsed<=1800 and stop is None and scripts_unchanged
            and all(r['freeze_control_passed'] for r in reports.values()))
    result=dict(status='complete' if len(reports)==18 and not stop else 'incomplete',elapsed_s=elapsed,
                stop_reason=stop,cells_observed=len(reports),cells_required=18,scripts_unchanged=scripts_unchanged,protocol_unchanged=protocol_unchanged,
                all_recording_controls_passed=passed,performance_qualified=False,
                campaign_manifest_sha256=harness.digest((output/'campaign-manifest.json').read_bytes()))
    (output/'campaign-completion.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result,indent=2))


if __name__=='__main__':
    p=argparse.ArgumentParser(__doc__);p.add_argument('candidate_build');p.add_argument('baseline_build');p.add_argument('new_output')
    a=p.parse_args();run(a.candidate_build,a.baseline_build,a.new_output)
