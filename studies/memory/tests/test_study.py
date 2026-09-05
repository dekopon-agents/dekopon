"""Deterministic safety tests. Temporary databases cannot launch through the public CLI."""
import io
import json
import multiprocessing as mp
import os
from pathlib import Path
import shutil
import sqlite3
import sys
import tarfile
import tempfile
import time
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from lib.ledger import Ledger, ROOT, Refusal, file_lock, canonical, digest, safe_path
from lib.executor import Executor
from lib.docker import Docker, Missing, TransportError
from lib.parse import ingest
from lib.recipes import prepare, validate_factors, prerequisite_artifacts, source_hash
from lib.publish import export, backup, restore, record_findings

class FakeEngine:
    id='unit-daemon'
    def __init__(self): self.objects={}; self.volumes={}; self.stops=[]; self.actual_id=None
    check_daemon=Docker.check_daemon
    def _call(self,method,path):
        assert (method,path)==('GET','/info')
        return {'ID':self.actual_id or self.id,'OSType':'linux','CgroupVersion':'2'}
    def inspect(self,name): return self.objects.get(name) or next((obj for obj in self.objects.values() if obj['Id']==name),None)
    verify=Docker.verify
    def stop(self,id):
        self.stops.append(id)
        for obj in self.objects.values():
            if obj['Id']==id: obj['State'].update(Running=False,Status='exited',ExitCode=137)
    def volume(self,name): return self.volumes.get(name)
    def logs(self,id): return b'synthetic test log'
    def archive(self,id,path): return b'synthetic test archive'
    def remove(self,id):
        for name,obj in list(self.objects.items()):
            if obj['Id']==id: del self.objects[name]
    def remove_volume(self,name): del self.volumes[name]

def claimant(root,campaign,queue,start):
    start.wait()
    db=Ledger(Path(root)); claim=Executor(db,FakeEngine()).claim(campaign,'infra')
    queue.put(claim['id'] if claim else None)

def owner_process(root,queue):
    db=Ledger(Path(root)); claim=Executor(db,FakeEngine()).claim('campaign','infra')
    with file_lock(db.private/'leases'/(claim['id']+'.lock')):
        queue.put(claim['id'])
        time.sleep(30)

class StudyTests(unittest.TestCase):
    def setUp(self):
        # Only disposable generated test fixtures are removed by TemporaryDirectory.
        # No registered worktree, source target, shared database or raw evidence is a candidate.
        parent=ROOT/'private/tests'; parent.mkdir(mode=0o700,parents=True,exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(prefix='fixture-',dir=parent)
        self.root=Path(self.temp.name).resolve()/'studies/memory'
        self.root.mkdir(parents=True)
        seam=self.root.parents[1]/'crates/dekopon-agent/src/prompt/history.rs'
        seam.parent.mkdir(parents=True)
        shutil.copyfile(ROOT.parents[1]/'crates/dekopon-agent/src/prompt/history.rs',seam)
        for f in ['study.py','matrix.json','DESIGN_DIGEST.md']:
            shutil.copyfile(ROOT/f,self.root/f)
        for d in ['lib','migrations','recipes']:
            shutil.copytree(ROOT/d,self.root/d,ignore=shutil.ignore_patterns('__pycache__'))
        self.db=Ledger(self.root); self.db.init()
        self.db.campaign('campaign',7,['BUILD-COLLECTOR','BUILD-HISTORY'],3)
        self.engine=FakeEngine(); self.ex=Executor(self.db,self.engine)
    def tearDown(self): self.temp.cleanup()
    def claim(self): return self.ex.claim('campaign','infra')
    def fake_live(self,id):
        r=self.ex.resource(id)
        obj={'Image':'unit-image','HostConfig':{},'Id':'c-'+id,'Name':'/'+r['container_name'],'Created':'created-identity',
            'Config':{'Labels':{'memory-study.token':r['token'],'memory-study.attempt':id}},
            'State':{'Running':True,'Restarting':False,'Dead':False,'Status':'running','StartedAt':'start-identity','ExitCode':0}}
        self.engine.objects[r['container_name']]=obj
        with self.db.transaction() as c:
            c.execute('UPDATE resource SET container_id=?,created_at=?,started_at=? WHERE attempt_id=?',(obj['Id'],obj['Created'],obj['State']['StartedAt'],id))
        return obj

    def producer(self,role,cell=None,mutate_source=False):
        cell=cell or ('BUILD-COLLECTOR-ARM64' if role=='collector' else 'BUILD-HISTORY-ARM64')
        campaign='producer-'+cell
        self.db.campaign(campaign,7,[cell],1)
        claim=self.ex.claim(campaign,'infra'); id=claim['id']; obj=self.fake_live(id)
        obj['State'].update(Running=False,Status='exited',ExitCode=0)
        self.ex.evidence(id,'harness-source','harness.tar',b'synthetic producer source archive')
        binary=bytearray(64); binary[:4]=b'\x7fELF';binary[18:20]=(183).to_bytes(2,'little')
        binary+=role.encode()
        out=io.BytesIO()
        with tarfile.open(fileobj=out,mode='w') as tar:
            t=tarfile.TarInfo(role);t.size=len(binary);tar.addfile(t,io.BytesIO(binary))
        with patch.object(self.engine,'logs',return_value=b'study-build-complete synthetic compiler version'), patch.object(self.engine,'archive',side_effect=lambda id,path: out.getvalue() if '/target/' in path else b'synthetic archive'):
            self.ex.drain(id)
            if mutate_source:
                with (self.root/'recipes/collector.c').open('a') as f:f.write('\n/* changed during build */\n')
            self.ex.retain(id,obj,role)
        self.ex.remove_owned(id);self.ex.finish(id,'succeeded',0)
        return self.db.rows('SELECT * FROM binary_product WHERE producer_attempt_id=?',(id,))[0]

    def test_endpoint_replacement_with_cached_id_preserves_original_lease(self):
        a=self.claim();obj=self.fake_live(a['id']); original=self.engine.id
        inspect=self.engine.inspect
        def replaced(name):
            self.engine.actual_id='replacement-daemon'
            return None  # HTTP 404 on the new daemon, original tree still alive.
        with patch.object(self.engine,'inspect',side_effect=replaced):
            self.assertTrue(self.ex.recover(a['id'])['lease_held'])
        self.assertEqual(self.engine.id,original)
        self.assertTrue(inspect(obj['Id'])['State']['Running'])
        self.assertFalse(self.engine.stops)
        self.assertTrue(self.db.rows('SELECT * FROM slot WHERE attempt_id=?',(a['id'],)))
        self.assertIsNone(self.ex.resource(a['id'])['drained_at'])

    def test_replacement_between_drain_and_deletion_or_absence_blocks(self):
        a=self.claim();self.fake_live(a['id']);self.ex.drain(a['id'])
        self.ex.evidence(a['id'],'raw-work','work.tar',b'synthetic archive')
        self.engine.actual_id='replacement'
        with self.assertRaisesRegex(Refusal,'daemon'):self.ex.remove_owned(a['id'])
        with self.assertRaisesRegex(Refusal,'daemon'):self.ex.finish(a['id'],'succeeded',0)
        self.assertTrue(self.engine.objects)
        self.assertTrue(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL'))

    def test_docker_404_revalidates_actual_endpoint_after_request(self):
        engine=object.__new__(Docker);engine.id='original'
        current=['original']
        def call(method,path,*args):
            if path=='/info':return {'ID':current[0],'OSType':'linux','CgroupVersion':'2'}
            current[0]='new-daemon';raise Missing('404 on replacement')
        with patch.object(engine,'_call',side_effect=call):
            with self.assertRaisesRegex(Refusal,'daemon identity'):engine.inspect('old-id')

    def test_factors_fail_closed_before_claim_and_history_args_are_explicit(self):
        experiments=self.db.rows("SELECT * FROM experiment WHERE recipe_id IN ('broker','history','history-smoke')")
        for exp in experiments:
            factors=json.loads(exp['config_json'])
            for field in [*factors,'unknown_factor']:
                changed=dict(exp); values=dict(factors);values[field]='unsupported-value';changed['config_json']=canonical(values)
                with self.subTest(cell=exp['id'],factor=field), self.assertRaisesRegex(Refusal,'unsupported factors'):
                    validate_factors(changed)
        self.db.campaign('bad-factors',1,['RT-X01'],1)
        with self.db.transaction() as c:c.execute("UPDATE experiment SET config_json=json_set(config_json,'$.idle_seconds',30) WHERE id='RT-X01'")
        self.assertIsNone(self.ex.claim('bad-factors','runtime'))
        self.assertFalse(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL'))
        self.assertIn('unsupported factors',self.db.rows('SELECT message FROM trial_incident')[-1]['message'])
        self.producer('collector');self.producer('history')
        exp=self.db.rows("SELECT * FROM experiment WHERE id='ST-X01-ARM64'")[0]
        with self.db.transaction() as c:artifacts=prerequisite_artifacts(self.db,c,exp)
        command,_=prepare(self.db,exp,artifacts)
        self.assertEqual(command[-6:],['128','12','65536','2048','4096','4'])

    def test_deadlines_are_in_container_commands_including_bootstrap_build(self):
        for exp in self.db.rows("SELECT * FROM experiment WHERE recipe_id IN ('build-history','build-collector','watchdog')"):
            command,_=prepare(self.db,exp,{})
            self.assertEqual(command[:3],['/usr/bin/timeout','--foreground','--signal=KILL'])
            self.assertEqual(int(command[3]),self.db.rows('SELECT timeout_s FROM recipe WHERE id=?',(exp['recipe_id'],))[0]['timeout_s'])
        self.producer('collector');self.producer('history')
        for exp in self.db.rows("SELECT * FROM experiment WHERE id IN ('INFRA-SMOKE','RT-X01','ST-X01-ARM64','INFRA-HISTORY')"):
            with self.db.transaction() as c:artifacts=prerequisite_artifacts(self.db,c,exp)
            command,_=prepare(self.db,exp,artifacts)
            self.assertEqual(command[:2],['/work/run/collector','--deadline'])
            self.assertGreater(int(command[2]),0)
        source=(self.root/'recipes/collector.c').read_text()
        self.assertIn('getpid()!=1',source)
        self.assertIn('static void deadline(int s) { (void)s; _exit(124); }',source)
        self.assertLess(source.index('alarm((unsigned)seconds)'),source.index('int checked=preflight()'))

    def test_versioned_binary_producer_and_consumed_input_links_survive_export(self):
        collector=self.producer('collector');history=self.producer('history')
        self.db.campaign('versions',8,['ST-X01','ST-X01-ARM64'],1)
        good=self.ex.claim('versions','state')
        self.assertEqual(good['experiment']['id'],'ST-X01-ARM64')
        self.assertEqual({r['artifact_id'] for r in self.db.rows('SELECT * FROM attempt_input WHERE attempt_id=?',(good['id'],))},{collector['artifact_id'],history['artifact_id']})
        self.ex.drain(good['id']);self.ex.finish(good['id'],'interrupted',None,'interrupted','synthetic',1)
        self.assertIsNone(self.ex.claim('versions','state'))
        self.assertIn('producer builder at current source',self.db.rows("SELECT message FROM trial_incident WHERE trial_id LIKE 'versions:ST-X01:%'")[0]['message'])
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute("INSERT INTO prerequisite_evidence VALUES('history',1,'absent','test')")
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute('UPDATE binary_product SET build_id=? WHERE artifact_id=?',('builder',history['artifact_id']))
        export(self.db)
        text=(self.root/'exports/snapshot.sql').read_text()
        public=sqlite3.connect(':memory:');public.executescript(text)
        rows=public.execute('SELECT p.build_id,p.source_id,b.compiler,s.git_sha,p.source_artifact_id,p.build_log_artifact_id FROM attempt_input i JOIN binary_product p ON p.artifact_id=i.artifact_id JOIN build b ON b.id=p.build_id LEFT JOIN source_revision s ON s.id=p.source_id').fetchall()
        self.assertEqual(len(rows),2)
        self.assertTrue(any(r[1]=='M' and r[3] for r in rows))
        self.assertTrue(all(r[2] and r[4] and r[5] for r in rows))
        self.assertFalse(public.execute('PRAGMA foreign_key_check').fetchall());public.close()

    def test_producer_source_changed_during_build_cannot_relabel_retained_binary(self):
        with self.assertRaisesRegex(Refusal,'producing driver changed'):
            self.producer('collector',mutate_source=True)
        self.assertTrue(self.db.rows("SELECT 1 FROM artifact WHERE kind='binary'"))
        self.assertFalse(self.db.rows('SELECT * FROM binary_product'))

    def test_old_binary_version_and_hash_do_not_become_prerequisites(self):
        self.producer('collector')
        exp=self.db.rows("SELECT * FROM experiment WHERE id='INFRA-SMOKE'")[0]
        with self.db.transaction() as c: self.assertTrue(prerequisite_artifacts(self.db,c,exp))
        with (self.root/'recipes/collector.c').open('a') as f:f.write('\n/* new version */\n')
        with self.assertRaisesRegex(Refusal,'current source'),self.db.transaction() as c:prerequisite_artifacts(self.db,c,exp)

    def test_primary_missing_dependency_survives_compound_drain_failure(self):
        a=self.claim()
        with patch('lib.executor.shutil.disk_usage',return_value=shutil._ntuple_diskusage(40*1024**3,4*1024**3,36*1024**3)),patch.object(self.engine,'image',create=True,side_effect=Missing('synthetic image dependency absent')),patch.object(self.engine,'inspect',side_effect=TransportError('synthetic drain unavailable')):
            result=self.ex.execute(a)
        self.assertTrue(result['lease_held'])
        errors=self.db.rows('SELECT category,message,retryable,role FROM error WHERE attempt_id=? ORDER BY id',(a['id'],))
        self.assertEqual(errors[0]['category'],'missing-dependency');self.assertEqual(errors[0]['retryable'],0)
        self.assertIn('image dependency absent',errors[0]['message'])
        self.assertEqual(errors[1]['role'],'cleanup');self.assertIn('drain unavailable',errors[1]['message'])
        self.assertEqual(self.ex.recover(a['id'])['status'],'blocked')
        self.assertEqual(self.ex.primary(a['id'])[2],'missing-dependency')

    def test_recovery_preserves_known_oom_nonzero_and_deadline_outcomes(self):
        for code,oom,category in [(137,True,'resource-exhaustion'),(124,False,'resource-exhaustion'),(9,False,'fixture-parser')]:
            a=self.claim();obj=self.fake_live(a['id']);obj['State'].update(Running=False,Status='exited',ExitCode=code,OOMKilled=oom)
            # Cleanup failure must not rewrite a permanent workload result as retryable.
            with patch.object(self.engine,'archive',side_effect=TransportError('synthetic archive failure')):
                self.assertTrue(self.ex.recover(a['id'])['lease_held'])
            self.assertEqual(self.ex.primary(a['id'])[2],category);self.assertEqual(self.ex.primary(a['id'])[4],0)
            self.assertEqual(self.ex.recover(a['id'])['status'],'failed')
            self.assertFalse(self.engine.stops)

    def test_transient_primary_retry_classification_ignores_cleanup_incidents(self):
        a=self.claim();self.ex.remember(a['id'],('failed',None,'transient','synthetic transport',1))
        self.ex.attention(a['id'],'secondary inspection unavailable')
        self.ex.recover(a['id'])
        tid=self.db.rows('SELECT trial_id FROM attempt WHERE id=?',(a['id'],))[0]['trial_id']
        with self.db.transaction() as c:
            c.execute("UPDATE trial SET status='blocked' WHERE id!=?",(tid,));c.execute("UPDATE trial SET status='pending' WHERE id=?",(tid,))
        self.assertIsNotNone(self.claim())

    def test_comparison_claim_order_is_randomized_and_sequence_is_recorded(self):
        self.producer('collector')
        self.db.campaign('comparison',20260905,['RT-X01','RT-X02'],3)
        expected=[r['id'] for r in self.db.rows("SELECT id FROM trial WHERE campaign_id='comparison' ORDER BY order_index")]
        actual=[]
        for _ in range(6):
            a=self.ex.claim('comparison','runtime');self.assertIsNotNone(a)
            actual.append(self.db.rows('SELECT trial_id FROM attempt WHERE id=?',(a['id'],))[0]['trial_id'])
            self.ex.drain(a['id']);self.ex.finish(a['id'],'succeeded',0)
        self.assertEqual(actual,expected)
        sequence=self.db.rows("SELECT a.trial_id FROM execution_sequence s JOIN attempt a ON a.id=s.attempt_id JOIN trial t ON t.id=a.trial_id WHERE t.campaign_id='comparison' ORDER BY s.sequence")
        self.assertEqual([r['trial_id'] for r in sequence],expected)
        self.assertTrue(any('RT-X02' in t for t in actual[:3]))

    def test_dependencies_override_random_order_not_all_variants(self):
        with self.db.transaction() as c:c.execute("INSERT INTO experiment_dependency VALUES('BUILD-COLLECTOR','BUILD-HISTORY')")
        self.db.campaign('dependency',3,['BUILD-COLLECTOR','BUILD-HISTORY'],1)
        a=self.ex.claim('dependency','infra');self.assertEqual(a['experiment']['id'],'BUILD-HISTORY')
        self.assertIsNone(self.ex.claim('dependency','infra'))
        self.ex.drain(a['id']);self.ex.finish(a['id'],'succeeded',0)
        self.assertEqual(self.ex.claim('dependency','infra')['experiment']['id'],'BUILD-COLLECTOR')

    def test_alternate_database_cannot_launch_live_backend(self):
        with self.assertRaisesRegex(Refusal,'canonical'):Executor(self.db).docker()

    def test_init_idempotent_matrix_typed_and_foreign_keys(self):
        self.assertFalse(self.db.init()['initialized'])
        self.assertEqual(len(self.db.rows('SELECT * FROM experiment')),32)
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute("INSERT INTO assignment VALUES('absent','keys','128')")
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute("INSERT INTO assignment VALUES(NULL,'keys','128')")
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute("INSERT INTO factor_level VALUES('keys','bad','text',NULL,NULL,'x')")
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:
            c.execute("INSERT INTO factor_level VALUES('trim','bad','boolean',NULL,NULL,NULL)")
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c: c.execute('INSERT INTO slot(id) VALUES(3)')
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c: c.execute('DELETE FROM slot WHERE id=2')

    def test_contention_across_overlapping_campaigns_exactly_two_claims(self):
        self.db.campaign('other',8,['BUILD-COLLECTOR'],3)
        ctx=mp.get_context('spawn'); queue=ctx.Queue(); start=ctx.Event()
        processes=[ctx.Process(target=claimant,args=(str(self.root),'other' if i%2 else 'campaign',queue,start)) for i in range(8)]
        for p in processes:p.start()
        start.set(); results=[queue.get(timeout=20) for _ in processes]
        for p in processes:p.join(20);self.assertEqual(p.exitcode,0)
        self.assertEqual(sum(x is not None for x in results),2)
        self.assertEqual(len(set(x for x in results if x)),2)
        self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL')),2)

    def test_live_owner_lock_refuses_recovery_then_killed_owner_can_recover(self):
        ctx=mp.get_context('spawn');q=ctx.Queue();p=ctx.Process(target=owner_process,args=(str(self.root),q));p.start()
        try:
            id=q.get(timeout=15)
            with self.assertRaisesRegex(Refusal,'active'): self.ex.recover(id)
            p.kill();p.join(10)
            self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id=?',(id,))),1)
            result=self.ex.recover(id)
            self.assertFalse(result['lease_held'])
            self.assertEqual(self.db.rows('SELECT status FROM attempt WHERE id=?',(id,))[0]['status'],'interrupted')
        finally:
            if p.is_alive():p.kill();p.join(10)

    def test_stale_heartbeat_does_not_release_live_tree(self):
        a=self.claim();obj=self.fake_live(a['id'])
        with self.db.transaction() as c:c.execute("UPDATE slot SET heartbeat_at='1900-01-01' WHERE attempt_id=?",(a['id'],))
        self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL')),1)
        result=self.ex.recover(a['id'])
        self.assertFalse(result['lease_held']);self.assertIn(obj['Id'],self.engine.stops)

    def test_stopped_snapshot_is_not_a_final_release_barrier(self):
        a=self.claim(); self.fake_live(a['id']); self.ex.drain(a['id'])
        with self.assertRaisesRegex(Refusal,'absence barrier'): self.ex.finish(a['id'],'succeeded',0)
        self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL')),1)

    def test_renamed_container_cannot_escape_identity_recovery(self):
        a=self.claim(); obj=self.fake_live(a['id']); r=self.ex.resource(a['id'])
        del self.engine.objects[r['container_name']]
        obj['Name']='/renamed-owned-container'; self.engine.objects['renamed-owned-container']=obj
        self.assertTrue(self.ex.recover(a['id'])['lease_held'])
        self.assertFalse(self.engine.stops)

    def test_start_identity_mismatch_blocks_recovery_and_preserves_slot(self):
        a=self.claim();obj=self.fake_live(a['id']);obj['State']['StartedAt']='different-start'
        result=self.ex.recover(a['id'])
        self.assertTrue(result['lease_held']);self.assertFalse(self.engine.stops)
        self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL')),1)

    def test_daemon_unavailable_or_replaced_never_releases(self):
        a=self.claim();self.engine.id='other-daemon'
        self.assertTrue(self.ex.recover(a['id'])['lease_held'])
        self.assertEqual(len(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL')),1)

    def test_release_requires_drain_evidence(self):
        a=self.claim()
        with self.assertRaisesRegex(Refusal,'undrained'):self.ex.finish(a['id'],'succeeded',0)
        self.ex.drain(a['id']);self.ex.finish(a['id'],'failed',1,'transient','simulated transport',1)
        self.assertFalse(self.db.rows('SELECT * FROM slot WHERE attempt_id IS NOT NULL'))

    def test_retry_preserves_predecessor_and_condition_and_two_attempt_cap(self):
        a=self.claim();self.ex.drain(a['id']);self.ex.finish(a['id'],'failed',1,'transient','simulated',1)
        tid=self.db.rows('SELECT trial_id FROM attempt WHERE id=?',(a['id'],))[0]['trial_id']
        with self.db.transaction() as c:
            c.execute("UPDATE trial SET status='blocked' WHERE id!=?",(tid,));c.execute("UPDATE trial SET status='pending' WHERE id=?",(tid,))
        b=self.claim();rows=self.db.rows('SELECT * FROM attempt WHERE trial_id=? ORDER BY number',(tid,))
        self.assertEqual(rows[1]['previous_id'],a['id']);self.assertEqual(rows[1]['condition_sha256'],rows[0]['condition_sha256'])
        self.ex.drain(b['id']);self.ex.finish(b['id'],'failed',1,'transient','simulated again',1)
        with self.db.transaction() as c:c.execute("UPDATE trial SET status='pending' WHERE id=?",(tid,))
        with self.assertRaisesRegex(Refusal,'two attempts'):self.claim()

    def test_changed_condition_not_an_invisible_retry(self):
        a=self.claim();self.ex.drain(a['id']);self.ex.finish(a['id'],'failed',1,'transient','simulated',1)
        tid=self.db.rows('SELECT trial_id FROM attempt WHERE id=?',(a['id'],))[0]['trial_id']
        with self.db.transaction() as c:
            c.execute("UPDATE trial SET status='blocked' WHERE id!=?",(tid,));c.execute("UPDATE trial SET status='pending' WHERE id=?",(tid,))
            c.execute('UPDATE experiment SET config_json=? WHERE id=?',('{"changed":true}',a['experiment']['id']))
        self.assertIsNone(self.claim())
        self.assertIn('unsupported factors',self.db.rows('SELECT message FROM trial_incident')[-1]['message'])

    def test_cleanup_refuses_unsafe_paths_active_target_and_foreign_volume(self):
        for path in ['../target','/target']:
            with self.assertRaises(Refusal):safe_path(self.root,path)
        (self.root/'link').symlink_to(self.root/'recipes',target_is_directory=True)
        with self.assertRaises(Refusal):safe_path(self.root,'link/collector.c')
        a=self.claim();obj=self.fake_live(a['id'])
        with self.assertRaises(Refusal):self.ex.remove_owned(a['id'])
        self.ex.drain(a['id'])
        with self.assertRaisesRegex(Refusal,'retained'):self.ex.remove_owned(a['id'])
        self.ex.evidence(a['id'],'raw-work','work.tar',b'synthetic test archive')
        r=self.ex.resource(a['id']);self.engine.volumes[r['volume_name']]={'Labels':{'memory-study.token':'foreign'}}
        with self.assertRaisesRegex(Refusal,'ownership'):self.ex.remove_owned(a['id'])
        self.assertIn(r['volume_name'],self.engine.volumes)

    def test_code_update_gate_and_canonical_db_copy_refusal(self):
        a=self.claim()
        with self.assertRaisesRegex(Refusal,'slots'):self.db.sync_driver()
        self.ex.drain(a['id']);self.ex.finish(a['id'],'interrupted',1,'interrupted','test',1)
        (self.root/'private/identity.json').write_text('{}')
        with self.assertRaisesRegex(Refusal,'identity'):self.db.connect()

    def history_attempt(self,campaign):
        self.db.campaign(campaign,7,['INFRA-HISTORY'],1)
        return self.ex.claim(campaign,'infra')['id']

    def test_history_phase_memory_reconstruction_and_legacy_whole_run_scope(self):
        self.producer('collector');self.producer('history')
        id=self.history_attempt('sync')
        env={'kind':'environment','os':'Linux','architecture':'aarch64','page_bytes':4096,'collector_libc':'static-test','docker_disk_free_bytes':10*1024**3,'memory_max':1073741824,'memory_swap_max':0,'pids_max':64,'cpu_quota':100000,'cpu_period':100000}
        phases=['load','retained','clones','clones-dropped','dropped','throughput'];events=[env]
        for i,phase in enumerate(phases,1):
            if phase in ('clones-dropped','dropped'):events.append({'kind':'phase-transition','phase':phase,'protocol':'history-pipe-v1'})
            events.append({'kind':'phase-sync','phase':phase,'protocol':'history-pipe-v1','elapsed_ns':i*100})
            events.append({'phase':phase,'elapsed_ns':i*100+1,'clock_origin':'observer-relative','metrics':{'process_rss_bytes':i*1000,'process_pss_bytes':i*900,'cgroup_memory_current_bytes':i*1200}})
            events.append({'phase':phase,'elapsed_ns':i,'clock_origin':'child-relative','metrics':{'retained_text_bytes':i*100}})
        events.append({'kind':'complete','exit_code':0})
        self.ex.evidence(id,'raw-log','output.log',b'\n'.join(canonical(e).encode() for e in events));ingest(self.db,id)
        self.assertEqual(self.db.rows('SELECT memory_scope FROM parse_scope')[0]['memory_scope'],'history-synchronized')
        rows=self.db.rows("SELECT phase,mean FROM phase_summary WHERE metric_id='process_rss_bytes'")
        self.assertEqual({r['phase']:r['mean'] for r in rows},{phase:i*1000 for i,phase in enumerate(phases,1)})
        clocks={r['clock_origin'] for r in self.db.rows('SELECT DISTINCT clock_origin FROM sample')}
        self.assertEqual(clocks,{'observer-relative','child-relative'})
        self.ex.drain(id);self.ex.finish(id,'succeeded',0)
        id=self.history_attempt('legacy')
        legacy=[env,{'phase':'startup','elapsed_ns':900,'metrics':{'process_rss_bytes':4321}},{'phase':'retained','elapsed_ns':1,'metrics':{'retained_text_bytes':9000}},{'kind':'complete','exit_code':0}]
        self.ex.evidence(id,'raw-log','output.log',b'\n'.join(canonical(e).encode() for e in legacy))
        with self.assertRaisesRegex(Refusal,'reconstruction unverified'):ingest(self.db,id)
        parsed=ingest(self.db,id,strict=False)
        self.assertEqual(self.db.rows('SELECT memory_scope FROM parse_scope WHERE parse_id=?',(parsed['parse_id'],))[0]['memory_scope'],'whole-run-only')
        self.assertEqual(self.db.rows("SELECT phase FROM sample WHERE parse_id=? AND metric_id='process_rss_bytes'",(parsed['parse_id'],))[0]['phase'],'whole-run')

    def test_history_phase_markers_without_deep_snapshots_fail_closed(self):
        self.producer('collector');self.producer('history');id=self.history_attempt('bad-sync')
        events=[]
        for i,phase in enumerate(['load','retained','clones','clones-dropped','dropped','throughput']):
            if i in (3,4):events.append({'kind':'phase-transition','phase':phase})
            events.append({'kind':'phase-sync','protocol':'history-pipe-v1','phase':phase,'elapsed_ns':i+1})
            events.append({'phase':phase,'elapsed_ns':i,'clock_origin':'child-relative','metrics':{'retained_text_bytes':1}})
        self.ex.evidence(id,'raw-log','output.log',b'\n'.join(canonical(e).encode() for e in events))
        with self.assertRaisesRegex(Refusal,'reconstruction unverified'):ingest(self.db,id)
        self.assertFalse(self.db.rows('SELECT * FROM parse_run'))

    def test_migration_preserves_existing_v3_snapshot_without_trusting_old_binaries(self):
        c=sqlite3.connect(':memory:')
        try:
            c.executescript('\n'.join((self.root/'migrations'/f'{n:03}.sql').read_text() for n in (1,2,3)))
            c.executescript("""
                INSERT INTO study VALUES('s','synthetic','source');
                INSERT INTO recipe VALUES('r','r','infra',2,1073741824,64,1,8388608);
                INSERT INTO experiment VALUES('e','s',NULL,'infra',0,0,'r',NULL,NULL,'ready','','{}');
                INSERT INTO campaign VALUES('c','test',7,printf('%064d',0),'open');
                INSERT INTO trial VALUES('t','c','e',1,0,NULL,'succeeded');
                INSERT INTO attempt(id,trial_id,number,status,started_at,driver_sha256,condition_sha256,command_json,parser_id) VALUES('a','t',1,'succeeded','test',printf('%064d',0),printf('%064d',0),'{}','old');
                INSERT INTO artifact VALUES('f','a','binary','private/raw/f.bin',printf('%064d',0),64,'private','test');
                INSERT INTO prerequisite VALUES('collector','legacy','artifact');
                INSERT INTO prerequisite_evidence VALUES('collector',1,'f','test');
                INSERT INTO metric VALUES('rss','byte','synthetic','gauge');
                INSERT INTO parse_run VALUES('p','a',printf('%064d',0),'f','test',NULL);
                INSERT INTO sample VALUES('p',0,'startup',1,'rss',123);
            """)
            self.assertEqual(c.execute('PRAGMA user_version').fetchone()[0],3)
            counts={t:c.execute('SELECT count(*) FROM '+t).fetchone()[0] for t in ('attempt','sample','artifact','prerequisite_evidence')}
            c.execute('PRAGMA foreign_keys=ON')
            c.executescript('BEGIN;'+(self.root/'migrations/004.sql').read_text()+'COMMIT;')
            self.assertEqual(counts,{t:c.execute('SELECT count(*) FROM '+t).fetchone()[0] for t in counts})
            self.assertEqual(c.execute('PRAGMA integrity_check').fetchone()[0],'ok')
            self.assertFalse(c.execute('PRAGMA foreign_key_check').fetchall())
            self.assertEqual(c.execute('SELECT count(*) FROM binary_product').fetchone()[0],0)
        finally:c.close()

    def test_parser_units_missing_metrics_immutable_raw_and_reparse(self):
        a=self.claim();id=a['id']
        env={'kind':'environment','os':'Linux','architecture':'aarch64','page_bytes':4096,'collector_libc':'static-test','docker_disk_free_bytes':10*1024**3,'memory_max':1073741824,'memory_swap_max':0,'pids_max':64,'cpu_quota':100000,'cpu_period':100000}
        events=[env,{'phase':'load','elapsed_ns':1,'metrics':{'process_rss_bytes':1234}},{'phase':'load','elapsed_ns':2,'metrics':{'process_rss_bytes':2468}},{'kind':'complete','exit_code':0}]
        self.ex.evidence(id,'raw-log','output.log',b'\n'.join(canonical(e).encode() for e in events))
        parsed=ingest(self.db,id);self.assertEqual(parsed['measurements'],2)
        self.assertFalse(ingest(self.db,id)['created'])
        self.assertEqual(self.db.rows('SELECT mean FROM phase_summary')[0]['mean'],1851)
        self.assertTrue(self.db.rows('SELECT * FROM missing_metric WHERE metric_id=?',('mallinfo2_uordblks_bytes',)))
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:c.execute("UPDATE artifact SET sha256=?",('f'*64,))
        with self.assertRaises(sqlite3.IntegrityError),self.db.transaction() as c:c.execute("INSERT INTO sample VALUES(?,99,'x',1,'unknown-unit',1,'legacy-unspecified')",(parsed['parse_id'],))
        with self.assertRaises(Refusal):self.ex.evidence(id,'raw-log','output.log',b'changed')

    def test_findings_are_idempotent_and_bad_evidence_rolls_back(self):
        f={'id':'unit-finding','status':'source-supported','claim':'synthetic','limitation':'test','evidence':[{'reference_id':'RT-H01-source'}]}
        (self.root/'findings.json').write_text(canonical([f]))
        record_findings(self.db,'findings.json');record_findings(self.db,'findings.json')
        self.assertEqual(len(self.db.rows("SELECT * FROM finding_evidence WHERE finding_id='unit-finding'")),1)
        f['id']='bad-reference';f['evidence']=[{'artifact_id':'absent'}]
        (self.root/'findings.json').write_text(canonical([f]))
        with self.assertRaises(sqlite3.IntegrityError):record_findings(self.db,'findings.json')
        self.assertFalse(self.db.rows("SELECT * FROM finding WHERE id='bad-reference'"))

    def test_backup_restore_preserves_current_ledger_and_refuses_fork(self):
        inode=self.db.path.stat().st_ino
        backup(self.db,'test')
        with self.db.transaction() as c: c.execute("INSERT INTO finding VALUES('later','unknown','synthetic','test')")
        result=restore(self.db,'test')
        self.assertTrue((self.root/result['preserved']/'ledger.sqlite3').exists())
        self.assertFalse(self.db.rows("SELECT * FROM finding WHERE id='later'"))
        self.assertEqual(self.db.path.stat().st_ino,inode)
        c=sqlite3.connect(self.root/'private/backups/test/ledger.sqlite3');c.execute("UPDATE executor_identity SET uuid='copied-other-executor'");c.commit();c.close()
        with self.assertRaises(Refusal):restore(self.db,'test')

    def test_export_refuses_authored_local_dns_without_overwriting_snapshot(self):
        export(self.db)
        before=(self.root/'exports/snapshot.sql').read_bytes()
        with self.db.transaction() as c:
            c.execute('INSERT INTO finding VALUES(?,?,?,?)',('private-fixture','unknown','synthetic-host.lan','Synthetic privacy sentinel, not real infrastructure.'))
        with self.assertRaisesRegex(Refusal,'private path/host sentinel'):
            export(self.db)
        self.assertEqual((self.root/'exports/snapshot.sql').read_bytes(),before)

    def test_export_reproducible_relational_and_not_an_executor(self):
        first=export(self.db);second=export(self.db)
        self.assertEqual(first['sha256'],second['sha256'])
        text=(self.root/'exports/snapshot.sql').read_text();self.assertNotIn(str(self.root),text)
        c=sqlite3.connect(':memory:');c.executescript(text);c.execute('PRAGMA foreign_keys=ON')
        self.assertEqual(c.execute('PRAGMA foreign_key_check').fetchall(),[])
        self.assertEqual(c.execute('PRAGMA user_version').fetchone()[0],self.db.migrations()[-1][0])
        self.assertEqual(c.execute('SELECT count(*) FROM executor_identity').fetchone()[0],0)
        self.assertEqual(c.execute('SELECT count(*) FROM experiment').fetchone()[0],32)
        c.close()

if __name__=='__main__':unittest.main()
