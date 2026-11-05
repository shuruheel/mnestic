import os,subprocess,time,json
from pathlib import Path
root=Path('/Users/shuruheel/Code/mindgraph/mnestic')
env=dict(os.environ,CARGO_TARGET_DIR='/tmp/mnestic-019-target',CARGO_PROFILE_TEST_DEBUG='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_PROFILE_RELEASE_DEBUG='0',CARGO_INCREMENTAL='0',ROCKSDB_LIB_DIR='/tmp/mnestic-019-target/debug/build/mnestic-rocks-ce2f3c38cec55330/out',ROCKSDB_STATIC='1')

def quiet():
 deadline=time.monotonic()+600
 while time.monotonic()<deadline:
  rows=subprocess.check_output(['ps','-axo','pid=,comm='],text=True).splitlines()
  busy=[r for r in rows if Path(r.strip().split(None,1)[-1]).name in {'cargo','rustc','clang++','clippy-driver'}]
  if not busy:return
  print('waiting for other native jobs:',[r.strip().split(None,1)[0] for r in busy],flush=True)
  time.sleep(10)
 raise RuntimeError('Native qualification deferred: other native jobs did not clear in 10 minutes')

def run(cmd,log,cwd=root):
 quiet();print('running',cmd,flush=True)
 with open(log,'w') as out:subprocess.run(cmd,cwd=cwd,env=env,stdout=out,stderr=subprocess.STDOUT,check=True,timeout=1800)

run(['cargo','test','--locked','-j','2','-p','mnestic','--features','storage-rocksdb,test-hooks','--test','fts_governed_stats','--test','fts_transaction_stats','--test','fts_stats_failures','--test','governed_transaction','--test','string_literals','--test','query_warnings'],'/tmp/mnestic-019-final-integration.log')
run(['cargo','clippy','--locked','-j','2','-p','mnestic','--features','storage-rocksdb,test-hooks','--lib','--test','string_literals','--test','query_warnings','--test','governed_transaction','--','-D','warnings'],'/tmp/mnestic-019-final-clippy.log')
host=Path('/tmp/mnestic-019-host')
p=host/'Cargo.toml';p.write_text(p.read_text().replace('../mnestic-019-packaged/','../mnestic-019-final-packaged/'))
(host/'src/main.rs').write_bytes(Path('/tmp/mnestic-019-final-packaged/mnestic-0.18.0/examples/governed_transaction.rs').read_bytes())
run(['cargo','run','--locked','-j','2'],'/tmp/mnestic-019-final-host.log',host)
subprocess.run(['/tmp/mnestic-019-bench-baseline/bin/python','-m','venv','/tmp/mnestic-019-sdist-python'],check=True)
for key in ('ROCKSDB_LIB_DIR','ROCKSDB_STATIC'):env.pop(key,None)
env['PYO3_USE_ABI3_FORWARD_COMPATIBILITY']='1'
env['CARGO_BUILD_JOBS']='2'
run(['/tmp/mnestic-019-sdist-python/bin/pip','install','/tmp/mnestic-019-sdist/mnestic-0.18.0.tar.gz'],'/tmp/mnestic-019-sdist-install.log')
env['MNESTIC_TEST_BACKENDS']='mem,sqlite'
for filename in ('test_string_json_contract.py','test_fts_transaction_stats.py'):
 run(['/tmp/mnestic-019-sdist-python/bin/python',str(root/'cozo-lib-python/tests'/filename)],'/tmp/mnestic-019-sdist-'+filename+'.log')
print('QUALIFICATION COMPLETE',flush=True)
