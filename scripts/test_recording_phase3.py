import gzip
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import benchmark_host as host
import overhead_controls as controls
import recording_phase3 as phase
import test_overhead_controls as previous_tests


class PhaseTests(unittest.TestCase):
    def test_all_blocks_required_and_identical_arms_have_unit_bounds(self):
        blocks = [([100+b*100+i for i in range(100)], [100+b*100+i for i in range(100)]) for b in range(10)]
        result = phase.estimates(blocks,draws=200)
        self.assertTrue(result['freeze_control_passed'])
        for e in result['estimates'].values():
            self.assertEqual(e['conservative_upper95'],1)
        with self.assertRaisesRegex(ValueError,'all ten'):
            phase.estimates(blocks[:-1],draws=1)

    def test_large_recording_cost_never_passes(self):
        result = phase.estimates([([120]*100,[100]*100)]*10,draws=200)
        self.assertFalse(result['freeze_control_passed'])
        self.assertEqual(result['estimates']['0.99']['conservative_upper95'],1.2)

    def test_archive_preserves_full_evidence_and_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)/'run'; root.mkdir()
            previous_tests.ControlsTests().fixture(root)
            original = controls.report(root,estimates=False)
            paired = phase.measured_pairs(root)
            files = phase.archive(root)
            self.assertFalse(list(root.glob('*.jsonl')))
            self.assertEqual(controls.report(root,estimates=False),original)
            self.assertEqual(phase.measured_pairs(root),paired)
            for name,item in files.items():
                with gzip.open(root/item['archive'],'rb') as data:
                    self.assertEqual(phase.harness.digest(data.read()),item['sha256'])
            # Missing measured samples remain invalid after compression.
            path=root/'records.jsonl.gz'
            with gzip.open(path,'rt') as data: rows=[json.loads(line) for line in data]
            with gzip.open(path,'wt') as data:
                for r in rows:
                    if r['input_id']!='pair-0-False': data.write(json.dumps(r)+'\n')
            self.assertFalse(controls.report(root,estimates=False)['freeze_control_passed'])
            with self.assertRaisesRegex(ValueError,'incomplete'):
                phase.measured_pairs(root)

    def test_native_disk_power_and_swap_conditions_fail_closed(self):
        valid=dict(free_mib=3000,ac_power=True,low_power_disabled=True,native=[],swapouts=10)
        self.assertIsNone(host.invalid(valid,10))
        for key,value,reason in [('free_mib',1000,'disk_limit'),('ac_power',False,'power_condition'),
                                  ('low_power_disabled',False,'power_condition'),('native',[(1,'cargo')],'competing_native_process'),
                                  ('swapouts',11,'swapout_change')]:
            self.assertEqual(host.invalid(dict(valid,**{key:value}),10),reason)
        self.assertEqual(host.swapouts('Swapouts: 123.\n'),123)
        with self.assertRaises(ValueError): host.swapouts('no counter')

    def fake_builds(self, root):
        builds={}
        for arm in ['candidate','baseline']:
            path=root/arm;path.mkdir();(path/'keyed_txtime_timing').write_bytes(arm.encode())
            info=dict(executable_sha256=phase.file_hash(path/'keyed_txtime_timing'),native={},rustc='same',
                      environment={'CARGO_TARGET_DIR':str(path/'target'),'RUSTFLAGS':None},
                      source=dict(head='272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea' if arm=='baseline' else 'candidate',files={
                          name:'same' for name in ['cozo-core/src/measurement.rs','cozo-core/examples/keyed_txtime/inputs.rs',
                                                  'cozo-core/examples/keyed_txtime_timing.rs','cozo-core/examples/keyed_txtime/overhead.rs']}))
            (path/'build.json').write_text(json.dumps(info))
            builds[arm]=dict(directory=str(path),executable_sha256=info['executable_sha256'],
                             build_manifest_sha256=phase.file_hash(path/'build.json'),build=info)
        review=dict(phase=3,implementation_review_passed=True,
                    builds={arm:dict(build_manifest_sha256=b['build_manifest_sha256'],executable_sha256=b['executable_sha256'],source=b['build']['source']) for arm,b in builds.items()},
                    shared_runner={name:{arm:'same' for arm in builds} for name in ['cozo-core/examples/keyed_txtime_timing.rs','cozo-core/examples/keyed_txtime/overhead.rs']})
        review_path=root/'review.json';review_path.write_text(json.dumps(review))
        return builds,review_path

    def test_distinct_targets_are_valid_but_manifest_drift_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            builds,review=self.fake_builds(Path(tmp))
            phase.validate_review(builds,json.loads(review.read_text()))
            self.assertEqual(phase.semantic_environment(builds['candidate']['build']),phase.semantic_environment(builds['baseline']['build']))
            phase.validate_frozen_builds(builds)
            path=Path(builds['candidate']['directory'])/'build.json'
            data=json.loads(path.read_text());data['rustc']='changed provenance';path.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError,'manifest changed'):
                phase.validate_frozen_builds(builds)

    def test_orchestration_stops_after_first_invalid_block(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);builds,review=self.fake_builds(root)
            with patch.object(phase.controls,'fixture',return_value={}), \
                 patch.object(phase.host,'preflight',return_value={'eligible':True,'final_sample':{'swapouts':0}}) as preflight, \
                 patch.object(phase.host,'Monitor'), patch.object(phase.harness,'supervise') as launch, \
                 patch.object(phase.controls,'report',return_value={'status':'incomplete'}):
                result=phase.run(root/'candidate',root/'baseline',root/'output',review)
            self.assertFalse(result['all_recording_controls_passed'])
            self.assertEqual(result['completed_blocks'],0)
            self.assertEqual(result['stop_reason']['reason'],'invalid_block')
            self.assertEqual(launch.call_count,1)
            self.assertEqual(preflight.call_count,1)

    def test_monitor_requires_five_consecutive_samples_for_same_busy_pid(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor=host.Monitor(Path(tmp)/'host.jsonl',tmp,10)
            self.addCleanup(monitor.close)
            sample=dict(free_mib=3000,ac_power=True,low_power_disabled=True,native=[],swapouts=10,busy=[(2,99,'busy')])
            with patch.object(host,'observation',return_value=sample), patch.object(host,'power',return_value={}):
                for _ in range(4): self.assertIsNone(monitor.observe([],set(),''))
                self.assertEqual(monitor.observe([],set(),''),'sustained_competing_cpu')
                sample['busy']=[]
                self.assertIsNone(monitor.observe([],set(),''))
                sample['busy']=[(3,99,'other')]
                self.assertIsNone(monitor.observe([],set(),''))


if __name__ == '__main__': unittest.main()
