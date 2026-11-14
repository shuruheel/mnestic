#!/usr/bin/env python3
"""Apply the shared timing harness to a CLEAN detached baseline worktree.

No optimizer/storage feature code is copied. Writes an auditable diff; does not
build, run, commit, fetch, or change the candidate worktree.
"""
import argparse
import hashlib
import difflib
import json
from pathlib import Path
import subprocess

BASE = '272c7b7196de9acd8d4aa66ec3e6e2f10f0057ea'


def replace_once(text, old, new):
    if text.count(old) != 1:
        raise ValueError(f'expected exactly one insertion anchor: {old!r}')
    return text.replace(old, new, 1)


def prepare(destination, evidence):
    source = Path(__file__).resolve().parents[1]
    destination = Path(destination).resolve()
    evidence = Path(evidence).resolve()
    git = lambda *args: subprocess.check_output(['git', '-C', str(destination), *args])
    if destination == source or git('rev-parse', 'HEAD').decode().strip() != BASE or git('status', '--porcelain'):
        raise ValueError('destination must be a clean worktree at the frozen baseline SHA')
    if evidence.exists():
        raise ValueError('evidence file already exists')
    files = {}
    for name in ['cozo-core/src/measurement.rs','cozo-core/examples/keyed_txtime_timing.rs','cozo-core/examples/keyed_txtime/overhead.rs', 'cozo-core/examples/keyed_txtime/inputs.rs']:
        files[name] = (source/name).read_text()
    # The baseline has no optimizer toggle. Refuse a toggle request rather than
    # silently label an identical run candidate-on/off. Timing code stays shared.
    runner = 'cozo-core/examples/keyed_txtime_timing.rs'
    files[runner] = replace_once(files[runner],
        '    if let Some(enabled) = plan.keyed_access {\n        db.set_keyed_access_for_tests(enabled);\n    }',
        '    ensure!(plan.keyed_access.is_none(), "baseline has no keyed-access toggle");')
    control = 'cozo-core/examples/keyed_txtime/overhead.rs'
    files[control] = replace_once(files[control],
        '                    if let Some(enabled) = plan.keyed_access {\n                        db.set_keyed_access_for_tests(enabled);\n                    }',
        '                    ensure!(plan.keyed_access.is_none(), "baseline has no keyed-access toggle");')
    name = 'cozo-core/Cargo.toml'
    files[name] = (destination/name).read_text() + '\n[[example]]\nname = "keyed_txtime_timing"\nrequired-features = ["test-hooks"]\n'
    name = 'cozo-core/src/lib.rs'
    files[name] = replace_once((destination/name).read_text(), 'pub(crate) mod storage;',
        '#[cfg(feature = "test-hooks")]\npub mod measurement;\npub(crate) mod storage;')
    name = 'cozo-core/src/runtime/db.rs'
    text = replace_once((destination/name).read_text(), '        // the real evaluation\n',
        '        // Compilation is complete; include evaluation and full output materialization.\n        #[cfg(feature = "test-hooks")]\n        let execution_span = crate::measurement::Span::new("query_execution");\n        // the real evaluation\n')
    files[name] = replace_once(text, '        result\n    }\n    pub(crate) fn list_running',
        '        #[cfg(feature = "test-hooks")]\n        if result.is_ok() { execution_span.finish(); }\n        result\n    }\n    pub(crate) fn list_running')
    name = 'cozo-core/src/runtime/transact.rs'
    text = replace_once((destination/name).read_text(), '    pub fn commit_tx(&mut self) -> Result<()> {\n',
        '    pub fn commit_tx(&mut self) -> Result<()> {\n        #[cfg(feature = "test-hooks")]\n        let commit_span = crate::measurement::Span::new("commit");\n')
    text = replace_once(text, '            return result;\n        }\n        self.projections.begin_commit',
        '            #[cfg(feature = "test-hooks")]\n            if result.is_ok() { commit_span.finish(); }\n            return result;\n        }\n        self.projections.begin_commit')
    files[name] = replace_once(text, '        res\n    }\n\n    fn commit_tx_inner',
        '        #[cfg(feature = "test-hooks")]\n        if res.is_ok() { commit_span.finish(); }\n        res\n    }\n\n    fn commit_tx_inner')
    # Validate all anchors before the first write; never half-apply on drift.
    for name, text in files.items():
        path = destination/name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    hashes = {name:hashlib.sha256(text.encode()).hexdigest() for name,text in files.items()}
    patch = git('diff', '--binary')
    for name in ['cozo-core/src/measurement.rs', 'cozo-core/examples/keyed_txtime_timing.rs', 'cozo-core/examples/keyed_txtime/overhead.rs', 'cozo-core/examples/keyed_txtime/inputs.rs']:
        addition = f'diff --git a/{name} b/{name}\nnew file mode 100644\n'
        addition += ''.join(difflib.unified_diff([], files[name].splitlines(keepends=True), fromfile='/dev/null', tofile='b/'+name))
        patch += addition.encode()
    evidence.write_text(json.dumps(dict(baseline=BASE, files=hashes,
        complete_patch_sha256=hashlib.sha256(patch).hexdigest(),
        baseline_only_difference='runner rejects keyed_access instead of calling absent optimizer toggle; no TxTime search instrumentation'), indent=2))
    evidence.with_suffix('.patch').write_bytes(patch)
    print(evidence)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('worktree'); parser.add_argument('evidence')
    args = parser.parse_args()
    prepare(args.worktree,args.evidence)
