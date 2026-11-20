#!/usr/bin/env python3
"""Read-only exploratory analysis of keyed/TxTime development traces."""
import argparse
from collections import defaultdict
import hashlib
import json
import math
from pathlib import Path
import random


def quantile(values, p):
    values = sorted(values)
    if not values:
        raise ValueError('empty quantile')
    return values[max(0, math.ceil(len(values)*p)-1)]


def records(directory):
    result = []
    for path in sorted(Path(directory).glob('*.jsonl')):
        for line in path.read_text().splitlines():
            row = json.loads(line)
            if 'timing' in row:
                result.append(row)
    return result


def overlaps(a,b):
    # Half-open intervals: touching endpoints alone do not establish overlap.
    return a[0] < a[1] and b[0] < b[1] and a[0] < b[1] and b[0] < a[1]


def covered_ns(intervals, window):
    intervals = sorted((max(a,window[0]), min(b,window[1])) for a,b in intervals if overlaps((a,b),window))
    total, end = 0, window[0]
    for a,b in intervals:
        total += max(0,b-max(a,end))
        end = max(end,b)
    return total


def build_analysis(rows, cell):
    ddl = [r for r in rows if r['cell']==cell and r['writer']==0]
    if len(ddl)!=1 or not ddl[0].get('timing'):
        raise ValueError('one captured DDL operation is required')
    events = ddl[0]['timing']['events']
    writers = [r for r in rows if r['cell']==cell and r['writer']>0 and r.get('timing')]
    waits, admissions, held_lower_bounds = [], [], []
    writer_waits = []
    for row in writers:
        es = row['timing']['events']
        complete_admission = [(e['start_ns'],e['end_ns']) for e in es
                              if e['phase'] in ['transaction_admission','relation_read_admission']]
        tt_waits = [(e['start_ns'],e['acquired_ns'] if e['acquired_ns'] is not None else e['end_ns'])
                    for e in es if e['phase']=='tt_commit_section']
        window = (row['timing']['start_ns'],row['timing']['end_ns'])
        writer_waits.append(dict(operation_id=row['operation_id'],writer=row['writer'],status=row['status'],
            admission_union_ns=covered_ns(complete_admission,window),tt_wait_union_ns=covered_ns(tt_waits,window),
            admission_and_tt_wait_union_ns=covered_ns(complete_admission+tt_waits,window)))
        for e in es:
            if e['phase']=='tt_commit_section':
                waits.append((e['start_ns'],e['acquired_ns'] if e['acquired_ns'] is not None else e['end_ns']))
            if e['phase']=='relation_read_admission':
                admissions.append((e['start_ns'],e['end_ns']))
        starts = [e['end_ns'] for e in es if e['phase']=='relation_read_admission' and e['completed']]
        commits = [e for e in es if e['phase']=='commit']
        ends = [e['end_ns'] for e in commits]
        if not starts and len(commits)==1:
            # Legacy traces lack admission; the commit funnel is still inside
            # execute_single's shared guard in these single-mutation fixtures.
            starts = [commits[0]['start_ns']]
        if len(starts)==len(ends)==1 and starts[0]<=ends[0]:
            # The synchronous execute_single guard is provably held throughout
            # this interval; it is released later, after commit returns.
            held_lower_bounds.append((starts[0],ends[0]))
    phases = []
    for e in events:
        if not e['phase'].startswith('build_'):
            continue
        window = (e['start_ns'],e['end_ns'])
        section_events = [s for s in events if s['phase']=='tt_ddl_section' and s['acquired_ns'] is not None]
        sections = [(max(s['acquired_ns'],window[0]),min(s['end_ns'],window[1])) for s in section_events if overlaps((s['acquired_ns'],s['end_ns']),window)]
        guaranteed = [(max(s['acquired_ns'],window[0]),min(s['release_begin_ns'],window[1])) for s in section_events
                      if s.get('release_begin_ns') is not None and overlaps((s['acquired_ns'],s['release_begin_ns']),window)]
        phases.append(dict(phase=e['phase'],completed=e['completed'],duration_ns=window[1]-window[0],
            commit_wait_envelope_overlaps=sum(overlaps(w,window) for w in waits),
            acquired_tt_sections=len(sections),
            commit_wait_potential_section_overlaps=(sum(any(overlaps(w,s) for s in sections) for w in waits) if sections else None),
            commit_wait_guaranteed_section_overlaps=(sum(any(overlaps(w,s) for s in guaranteed) for w in waits) if guaranteed else None),
            read_admission_envelope_overlaps=sum(overlaps(a,window) for a in admissions),
            scripts_inflight=sum(overlaps((r['timing']['start_ns'],r['timing']['end_ns']),window) for r in writers)))
    relation_waits=[]
    for e in events:
        if e['phase']=='relation_write_admission':
            window=(e['start_ns'],e['end_ns'])
            covered=covered_ns(held_lower_bounds,window)
            relation_waits.append(dict(completed=e['completed'],duration_ns=window[1]-window[0],
                shared_guard_covered_ns=covered,
                shared_guard_covered_fraction=covered/(window[1]-window[0]) if window[1]>window[0] else None))
    ddl_window=(ddl[0]['timing']['start_ns'],ddl[0]['timing']['end_ns'])
    ddl_covered=covered_ns(held_lower_bounds,ddl_window)
    return dict(cell=cell,ddl_status=ddl[0]['status'],ddl_ns=ddl[0]['end_to_end_ns'],
                ddl_shared_guard_covered_fraction=ddl_covered/(ddl_window[1]-ddl_window[0]),
                shared_guard_basis='admission-end to commit-end when available; otherwise commit interval',
                phases=phases,relation_write_waits=relation_waits,
                maintenance_admissions=[dict(completed=e['completed'],duration_ns=e['end_ns']-e['start_ns'])
                                        for e in events if e['phase']=='maintenance_admission'],
                writer_waits=writer_waits,
                max_writer_admission_and_tt_wait_union_ns=max((w['admission_and_tt_wait_union_ns'] for w in writer_waits),default=0),
                writer_operations=len(writers),writer_errors=sum(r['status']!='ok' for r in writers),
                interpretation='development diagnostic; phase envelopes are not acquired critical sections')


def overhead_analysis(rows, draws=10000):
    groups=defaultdict(dict)
    for row in rows:
        if not row['cell'].startswith('overhead_'):
            continue
        _,pair,enabled=row['input_id'].split('-')
        key=(row['writer'],enabled)
        if key in groups[int(pair)]:
            raise ValueError('duplicate overhead operation')
        if row['status']!='ok' or not row.get('oracle_ok') or not row.get('trace_valid') or not row.get('timing'):
            raise ValueError('failed overhead operation; no successful-subset estimate')
        groups[int(pair)][key]=row
    expected={(w,e) for w in range(1,5) for e in ['True','False']}
    if set(groups)!=set(range(100)) or any(set(g)!=expected for g in groups.values()):
        raise ValueError('require all 100 paired four-writer rounds')
    on,off=[],[]
    rank_changes=0
    changed_rounds=0
    for i in range(100):
        ranks={}
        for label,values in [('True',on),('False',off)]:
            rs=[groups[i][(w,label)] for w in range(1,5)]
            values.append(max(r['timing']['end_ns'] for r in rs)-min(r['timing']['start_ns'] for r in rs))
            ranks[label]={r['writer']:rank for rank,r in enumerate(sorted(rs,key=lambda r:(r['timing']['end_ns'],r['writer'])))}
        changed=sum(ranks['True'][w]!=ranks['False'][w] for w in range(1,5))
        rank_changes+=changed
        changed_rounds+=changed>0
    rng=random.Random(20261114)
    boot={.5:[],.99:[]}
    for _ in range(draws):
        starts=[rng.randrange(10)*10 for _ in range(10)]
        indices=[i for start in starts for i in range(start,start+10)]
        a=[on[i] for i in indices];b=[off[i] for i in indices]
        for q in boot: boot[q].append(quantile(a,q)/quantile(b,q))
    estimates={str(q):dict(on_ns=quantile(on,q),off_ns=quantile(off,q),ratio=quantile(on,q)/quantile(off,q),
                          percentile_95_interval=[quantile(boot[q],.025),quantile(boot[q],.975)]) for q in boot}
    return dict(pairs=100,round_statistic='max(end_ns)-min(start_ns), all four writers',
                quantiles='nearest rank',bootstrap=dict(draws=draws,seed=20261114,paired_round_block_size=10),
                estimates=estimates,writer_finish_rank_changes=rank_changes,writer_pairs=400,
                rounds_with_rank_changes=changed_rounds,
                limitation='Exploratory single-trajectory analysis; mutations and index histories differ between conditions. Not an isolated instrumentation-cost estimate.')


def main():
    p=argparse.ArgumentParser(__doc__)
    p.add_argument('directory',type=Path)
    p.add_argument('--cell')
    p.add_argument('--overhead',action='store_true')
    a=p.parse_args()
    rows=records(a.directory)
    result=overhead_analysis(rows) if a.overhead else build_analysis(rows,a.cell)
    result['input_sha256']={f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(a.directory.glob('*.jsonl'))}
    print(json.dumps(result,sort_keys=True,indent=2))


if __name__=='__main__':
    main()
