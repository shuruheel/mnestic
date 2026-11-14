import json
from pathlib import Path
import tempfile
import hashlib
import unittest
import overhead_controls as controls


class ControlsTests(unittest.TestCase):
    def fixture(self, root):
        plan=controls.fixture('rocksdb',False,'read')
        (root/'plan.json').write_text(json.dumps(plan))
        (root/'summary.json').write_text(json.dumps({'development_conformance':True}))
        root.with_suffix('.completion.json').write_text(json.dumps({'exit_code':0,'stop_reason':None,'elapsed_s':1}))
        binary=root/'runner';binary.write_bytes(b'unit fixture')
        digest=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
        build={'executable':str(binary),'executable_sha256':digest(binary)}
        (root/'build.json').write_text(json.dumps(build))
        manifest={'plan_sha256':digest(root/'plan.json'),'executable_sha256':digest(binary)}
        (root/'manifest.json').write_text(json.dumps(manifest))
        root.with_suffix('.supervisor.json').write_text(json.dumps(dict(build_manifest=build,
            build_manifest_sha256=digest(root/'build.json'),binary_sha256=digest(binary),
            plan_sha256=digest(root/'plan.json'),limits={'seconds':180,'rss_mib':4096,'min_free_mib':2048})))
        root.with_suffix('.resources.csv').write_text('rss_kib,free_mib\n100,3000\n')
        files={'database':'a'*64}
        snapshots=[{'kind':'seed','files':files}]
        rows=[]
        def add(cell,writer,name):
            n=len(rows)
            rows.append(dict(cell=cell,writer=writer,input_id=name,operation_id=n,status='ok',
                             oracle_ok=True,trace_valid=True,output_sha256='same',
                             timing={'start_ns':n*1000,'end_ns':n*1000+100}))
        for op in plan['setup']: add('setup',0,op['id'])
        for pair in range(100):
            for enabled in [True,False]:
                label=str(enabled)
                snapshots.extend([dict(kind='arm_start',pair=pair,enabled=enabled,files=files),
                                  dict(kind='arm_end',pair=pair,enabled=enabled,files=files,copy_ns=1,validation_ns=1,open_ns=1,close_ns=1,final_inventory_ns=1,cleanup_ns=1,warmups=[{'worker':1,'start_ns':0,'end_ns':0}])])
                for i in range(100): add('overhead',1,f'warmup-{pair}-{label}-{i}')
                add('overhead',1,f'pair-{pair}-{label}')
                add('overhead_verify',0,f'verify-{pair}-{label}-0')
        self.save(root/'records.jsonl',rows)
        self.save(root/'snapshots.jsonl',snapshots)
        return rows,snapshots

    def save(self,path,rows): path.write_text(''.join(json.dumps(r)+'\n' for r in rows))

    def test_complete_matched_pairs_report_unit_ratio(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)/'run';root.mkdir();self.fixture(root)
            result=controls.report(root)
            self.assertEqual(result['status'],'complete')
            self.assertTrue(result['freeze_control_passed'])
            self.assertEqual(result['estimates']['0.99']['conservative_upper95'],1)

    def test_missing_measured_pair_suppresses_estimates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)/'run';root.mkdir();rows,_=self.fixture(root)
            rows=[r for r in rows if r['input_id']!='pair-0-False']
            self.save(root/'records.jsonl',rows)
            result=controls.report(root)
            self.assertEqual(result['missing'],1)
            self.assertNotIn('estimates',result)
            self.assertFalse(result['freeze_control_passed'])

    def test_snapshot_or_logical_state_mismatch_suppresses_estimates(self):
        for mismatch in ['snapshot','logical']:
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as tmp:
                root=Path(tmp)/'run';root.mkdir();rows,snapshots=self.fixture(root)
                if mismatch=='snapshot':
                    snapshots[1]['files']={'database':'b'*64}
                    self.save(root/'snapshots.jsonl',snapshots)
                else:
                    next(r for r in rows if r['input_id']=='verify-0-False-0')['output_sha256']='different'
                    self.save(root/'records.jsonl',rows)
                result=controls.report(root)
                self.assertEqual(result['status'],'incomplete')
                self.assertNotIn('estimates',result)

    def test_unfrozen_plan_and_exceeded_budget_suppress_estimates(self):
        for mismatch in ['plan','budget']:
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as tmp:
                root=Path(tmp)/'run';root.mkdir();self.fixture(root)
                if mismatch=='plan':
                    path=root/'plan.json';path.write_text(path.read_text()+' ')
                else:
                    root.with_suffix('.completion.json').write_text(json.dumps({'exit_code':0,'elapsed_s':181}))
                result=controls.report(root)
                self.assertEqual(result['status'],'incomplete')
                self.assertNotIn('estimates',result)

    def test_mutations_preserve_vectors_and_use_disjoint_targets(self):
        plan=controls.fixture('rocksdb',True,'contention')
        initial={r[0]:r for r in plan['setup'][1]['params']['r']}
        for round_ in plan['overhead_control']['rounds']:
            targets=[]
            for op in round_['workers']:
                row=op['params']['r'][0];targets.append(row[0])
                self.assertEqual(row[2:],initial[row[0]][2:])
                self.assertNotEqual(row[1],initial[row[0]][1])
            self.assertEqual(len(set(targets)),4)


if __name__=='__main__': unittest.main()
