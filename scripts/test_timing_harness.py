"""Regression tests for fail-closed development evidence reporting."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('timing_harness', Path(__file__).with_name('timing_harness.py'))
H = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(H)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)/'run'
        self.root.mkdir()
        self.plan = dict(setup=[],verify=[],steps=[dict(mode='serial',cell='overhead',operations=[])])
        self.rows = []
        for i in range(100):
            for enabled in ['True','False']:
                name = f'pair-{i}-{enabled}'
                self.plan['steps'][0]['operations'].append(dict(id=name))
                self.rows.append(dict(cell='overhead',writer=0,input_id=name,operation_id=len(self.rows),status='ok',
                                     oracle_ok=True,trace_valid=True,end_to_end_ns=110 if enabled=='True' else 100,
                                     timing=dict(start_ns=0,end_ns=110,events=[])))

    def write(self):
        (self.root/'plan.json').write_text(json.dumps(self.plan))
        (self.root/'summary.json').write_text(json.dumps(dict(development_conformance=True)))
        self.root.with_suffix('.completion.json').write_text(json.dumps(dict(exit_code=0,stop_reason=None)))
        (self.root/'raw.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in self.rows))

    def test_complete_pairs_report_expected_ratio(self):
        self.write()
        r=H.report(self.root)
        self.assertEqual(r['status'],'complete')
        self.assertEqual(r['overhead_pairs_valid'],100)
        self.assertEqual(r['overhead_p50_ratio'],1.1)
        self.assertFalse(r['performance_qualified'])

    def test_missing_or_duplicate_pairs_suppress_estimates(self):
        for mutation in ['missing','duplicate']:
            with self.subTest(mutation=mutation):
                original=copy.deepcopy(self.rows)
                if mutation=='missing': self.rows.pop()
                else: self.rows.append(self.rows[-1])
                self.write()
                r=H.report(self.root)
                self.assertEqual(r['status'],'incomplete')
                self.assertIsNone(r['overhead_p50_ratio'])
                self.rows=original

    def test_panic_is_retained_without_crashing_report(self):
        row=self.rows[-1]
        for name in ['end_to_end_ns','oracle_ok','trace_valid']:
            row.pop(name)
        row.update(status='panic',timing=None)
        self.write()
        r=H.report(self.root)
        self.assertEqual(r['status'],'incomplete')
        self.assertEqual(r['oracle_or_trace_failures'],1)
        self.assertIsNone(r['overhead_p50_ratio'])

    def test_empty_or_interrupted_run_is_incomplete(self):
        self.assertEqual(H.report(self.root)['status'],'incomplete')
        self.write()
        (self.root/'summary.json').unlink()
        self.assertEqual(H.report(self.root)['status'],'incomplete')

    def test_setup_cannot_overwrite_build_phase_coverage(self):
        self.plan['steps'].append(dict(mode='build',cell='build_fts',ddl=dict(id='ddl'),
                                      workers=[[dict(id='write')]]*4,max_attempts_per_writer=10))
        self.plan['setup'].append(dict(id='setup-index'))
        def row(cell,writer,name,id,events,start,end):
            return dict(cell=cell,writer=writer,input_id=name,operation_id=id,status='ok',oracle_ok=True,trace_valid=True,
                        timing=dict(start_ns=start,end_ns=end,events=events))
        def event(phase,start,end,acquired=None):
            return dict(phase=phase,start_ns=start,end_ns=end,acquired_ns=acquired)
        self.rows.append(row('build_fts',0,'ddl',300,[event('build_registration',10,20)],10,20))
        self.rows.append(row('setup',0,'setup-index',301,[event('build_registration',1,2)],1,2))
        for w in range(1,5):
            self.rows.append(row('build_fts',w,'write',400+w*2,[event('tt_commit_section',12,25,22)],12,25))
            self.rows.append(row('build_fts',w,'write',401+w*2,[event('tt_commit_section',30,35,32)],30,35))
        self.write()
        r=H.report(self.root)
        self.assertEqual(r['status'],'complete')
        self.assertEqual(r['phase_coverage'],{'build_fts/300/build_registration':dict(writer_attempts_overlapping=4,covered=True)})
        self.rows.pop()
        self.write()
        self.assertEqual(H.report(self.root)['status'],'incomplete')

    def test_binary_manifest_mismatch_prevents_launch(self):
        binary=Path(self.temp.name)/'binary'; binary.write_text('not executable')
        manifest=Path(self.temp.name)/'build.json';manifest.write_text(json.dumps(dict(executable_sha256='wrong')))
        with self.assertRaisesRegex(ValueError,'binary does not match'):
            H.supervise(binary,Path(self.temp.name)/'plan',Path(self.temp.name)/'new-run',manifest,1,4096,2048)
        self.assertFalse((Path(self.temp.name)/'new-run.supervisor.json').exists())


if __name__=='__main__':
    unittest.main()
