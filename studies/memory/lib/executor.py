"""Transactional two-slot executor. A stale heartbeat is never permission to release."""
import io
import json
import os
from pathlib import Path
import shutil
import signal
import tarfile
import time
import uuid
from .ledger import ROOT, Refusal, canonical, digest, driver_hash, file_lock, safe_path, utc, GIB
from .docker import Docker, EngineError, TransportError, Missing, archive_file
from .recipes import prerequisite_artifacts, prepare, validate_factors, source_hash

class Executor:
    def __init__(self,db,engine=None):
        self.db=db; self.engine=engine
    def docker(self):
        if self.db.root!=ROOT and (self.engine is None or isinstance(self.engine,Docker)):
            raise Refusal('live backend requires the canonical database; alternate roots are test-only')
        if self.engine is None: self.engine=Docker()
        return self.engine

    def claim(self,campaign,lane):
        engine=self.docker(); engine.check_daemon()
        with self.db.transaction() as c:
            dh=driver_hash(self.db.root)
            cp=c.execute('SELECT * FROM campaign WHERE id=?',(campaign,)).fetchone()
            if not cp or cp['status']!='open' or cp['driver_sha256']!=dh or c.execute('SELECT driver_sha256 FROM executor_identity').fetchone()[0]!=dh: raise Refusal('campaign/driver identity mismatch')
            slot=c.execute('SELECT id FROM slot WHERE attempt_id IS NULL ORDER BY id LIMIT 1').fetchone()
            if not slot: return None
            trials=c.execute('SELECT t.* FROM trial t JOIN experiment e ON e.id=t.experiment_id WHERE t.campaign_id=? AND e.lane=? AND t.status=\'pending\' ORDER BY COALESCE((SELECT min(peer.priority) FROM comparison_member cm JOIN comparison_member peers ON peers.group_id=cm.group_id JOIN experiment peer ON peer.id=peers.experiment_id WHERE cm.experiment_id=e.id),e.priority),t.order_index',(campaign,lane)).fetchall()
            for trial in trials:
                exp=dict(c.execute('SELECT * FROM experiment WHERE id=?',(trial['experiment_id'],)).fetchone())
                if exp['status']!='ready':
                    c.execute("UPDATE trial SET status='blocked' WHERE id=?",(trial['id'],)); continue
                predecessors=c.execute('SELECT predecessor_id FROM experiment_dependency WHERE experiment_id=?',(exp['id'],)).fetchall()
                if any(not c.execute("SELECT 1 FROM trial WHERE campaign_id=? AND experiment_id=? AND replicate=? AND status='succeeded'",(campaign,dep[0],trial['replicate'])).fetchone() for dep in predecessors): continue
                try:
                    validate_factors(exp)
                    artifacts=prerequisite_artifacts(self.db,c,exp)
                    recipe=dict(c.execute('SELECT * FROM recipe WHERE id=?',(exp['recipe_id'],)).fetchone())
                    build=dict(c.execute('SELECT * FROM build WHERE id=?',(exp['build_id'],)).fetchone())
                    build.pop('binary_sha256',None);build.pop('symbols_sha256',None)
                    cmd,files=prepare(self.db,exp,artifacts)
                except (Refusal,OSError) as e:
                    # No fabricated attempt: retain the actual pre-claim blocker and continue.
                    c.execute("UPDATE trial SET status='blocked' WHERE id=?",(trial['id'],))
                    c.execute('INSERT INTO trial_incident(trial_id,stage,category,message,at) VALUES(?,?,?,?,?)',(trial['id'],'pre-claim',self.classify(error=e)[2],type(e).__name__+': '+str(e)[:400],utc()))
                    continue
                condition={'experiment':exp,'recipe':recipe,'build':build,'daemon_id':engine.id,'command':cmd,'input_hashes':{k:digest(v[0]) for k,v in files.items()}}
                ch=digest(canonical(condition).encode())
                previous=c.execute('SELECT * FROM attempt WHERE trial_id=? ORDER BY number DESC LIMIT 1',(trial['id'],)).fetchone()
                number=previous['number']+1 if previous else 1
                if number>2: raise Refusal('maximum two attempts per trial')
                if previous:
                    err=c.execute("SELECT retryable FROM error WHERE attempt_id=? AND role='primary' ORDER BY id LIMIT 1",(previous['id'],)).fetchone()
                    if not err or not err[0]: raise Refusal('non-transient retry refused')
                id=uuid.uuid4().hex; token=uuid.uuid4().hex
                c.execute('INSERT INTO attempt(id,trial_id,number,previous_id,status,started_at,driver_sha256,condition_sha256,command_json,parser_id) VALUES(?,?,?,?,?,?,?,?,?,?)',(id,trial['id'],number,previous['id'] if previous else None,'claimed',utc(),dh,ch,canonical({'command':cmd,'condition':condition}),'jsonl-v1'))
                c.execute('INSERT INTO execution_sequence(attempt_id,claimed_at) VALUES(?,?)',(id,utc()))
                for role,artifact in artifacts.items():
                    c.execute('INSERT INTO attempt_input VALUES(?,?,?)',(id,role,artifact['id']))
                c.execute('UPDATE slot SET attempt_id=?,token=?,heartbeat_at=? WHERE id=?',(id,token,utc(),slot['id']))
                c.execute("UPDATE trial SET status='running' WHERE id=?",(trial['id'],))
                c.execute('INSERT INTO resource(attempt_id,token,daemon_id,container_name,volume_name) VALUES(?,?,?,?,?)',(id,token,engine.id,'memory-study-'+id,'memory-study-'+id+'-work'))
                return {'id':id,'token':token,'driver_sha256':dh,'experiment':exp,'recipe':recipe,'build':build,'command':cmd,'files':files}
            return None

    def resource(self,id):
        rows=self.db.rows('SELECT * FROM resource WHERE attempt_id=?',(id,))
        if not rows: raise Refusal('no registered resource')
        return rows[0]

    def active(self,c,id,token):
        if not c.execute('SELECT 1 FROM slot WHERE attempt_id=? AND token=?',(id,token)).fetchone(): raise Refusal('lease lost or already recovered; launch refused')

    def evidence(self,id,kind,name,data):
        relative='private/raw/'+id+'/'+name
        with self.db.transaction() as c:
            old=c.execute('SELECT * FROM artifact WHERE relative_path=?',(relative,)).fetchone()
            if old:
                if old['sha256']!=digest(data) or digest(safe_path(self.db.root,relative,True).read_bytes())!=old['sha256']:
                    raise Refusal('immutable evidence conflict; retain tree for manual salvage')
                return old['id']
            return self.db.artifact(c,id,kind,relative,data)

    def cleanup_receipt(self,id,target,action,reason,before=None,after=None):
        with self.db.transaction() as c:
            c.execute('INSERT INTO cleanup(attempt_id,target,action,reason,free_before_bytes,free_after_bytes,at) VALUES(?,?,?,?,?,?,?)',(id,target,action,reason,before,after,utc()))

    def drain(self,id):
        engine=self.docker(); r=self.resource(id)
        obj=self.snapshot(id)
        if obj is not None:
            engine.verify(obj,r)
            if obj['State']['Running'] or obj['State'].get('Restarting'):
                engine.stop(obj['Id'])
            for _ in range(25):
                obj=self.snapshot(id)
                if obj is None: raise Refusal('container vanished during drain; do not infer identity')
                engine.verify(obj,r)
                if not obj['State']['Running'] and not obj['State'].get('Restarting') and not obj['State'].get('Dead') and obj['State']['Status'] in ('created','exited'):
                    break
                time.sleep(.2)
            else: raise Refusal('container stop unverified; lease held')
        engine.check_daemon(r['daemon_id'])
        with self.db.transaction() as c:
            c.execute('UPDATE resource SET drained_at=? WHERE attempt_id=?',(utc(),id))
            c.execute('INSERT INTO cleanup(attempt_id,target,action,reason,at) VALUES(?,?,?,?,?)',(id,r['container_name'],'drained','Same daemon+token+container/create/start identity stopped (or never created); PID namespace/cgroup lifetime owns all descendants.',utc()))
        return obj

    def retain(self,id,obj,build_kind=None):
        engine=self.docker(); r=self.resource(id)
        if obj is None:
            self.cleanup_receipt(id,r['volume_name'],'retained','No container archive endpoint; retain any partially created volume.')
            return None
        logs=engine.logs(obj['Id'])
        log_id=self.evidence(id,'raw-log','output.log',logs)
        # Own synthetic files, not a rootfs dump. Hard maximum applies before storing/decoding.
        work=engine.archive(obj['Id'],'/work')
        work_id=self.evidence(id,'raw-work','work.tar',work)
        self.evidence(id,'metadata','container-final.json',canonical({'Id':obj['Id'],'Created':obj['Created'],'Image':obj['Image'],'State':obj['State'],'HostConfig':{k:obj['HostConfig'].get(k) for k in ['Memory','MemorySwap','NanoCpus','PidsLimit','ReadonlyRootfs','NetworkMode']}}).encode())
        if build_kind and obj['State']['ExitCode']==0:
            if b'study-build-complete' not in logs: raise Refusal('build success marker missing')
            binary=archive_file(engine.archive(obj['Id'],'/work/run/target/'+build_kind),build_kind)
            if not binary.startswith(b'\x7fELF') or len(binary)<64 or int.from_bytes(binary[18:20],'little')!=183: raise Refusal('non-ELF build refused')
            binary_id=self.evidence(id,'binary',build_kind+'.bin',binary)
            with self.db.transaction() as c:
                producer=c.execute('SELECT driver_sha256 FROM attempt WHERE id=?',(id,)).fetchone()
                if producer[0]!=driver_hash(self.db.root): raise Refusal('producing driver changed; retained binary cannot become a prerequisite')
                exp=c.execute('SELECT e.* FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id WHERE a.id=?',(id,)).fetchone()
                source=c.execute("SELECT id FROM artifact WHERE attempt_id=? AND kind='harness-source'",(id,)).fetchone()
                if not source: raise Refusal('missing producing source archive')
                version=source_hash(self.db,build_kind)
                source_id='M' if build_kind=='history' else None
                c.execute('INSERT OR IGNORE INTO binary_product VALUES(?,?,?,?,?,?,?,?)',(binary_id,id,build_kind,exp['build_id'],source_id,version,work_id,log_id))
                prerequisite=build_kind+':'+exp['build_id']+':'+version
                c.execute('INSERT OR IGNORE INTO prerequisite VALUES(?,?,?)',(prerequisite,'Exact producing build and source version','artifact'))
                c.execute('INSERT INTO prerequisite_evidence VALUES(?,1,?,?) ON CONFLICT(prerequisite_id) DO UPDATE SET evidence=excluded.evidence,updated_at=excluded.updated_at',(prerequisite,binary_id,utc()))
        return logs

    def remove_owned(self,id):
        engine=self.docker(); r=self.resource(id)
        free_before=shutil.disk_usage(self.db.root).free
        engine.check_daemon(r['daemon_id'])
        if not r['drained_at']: raise Refusal('no verified drain identity')
        # Binary and complete work archive must be immutable before reclaiming any build/data volume.
        artifacts=self.db.rows('SELECT * FROM artifact WHERE attempt_id=?',(id,))
        if not any(a['kind']=='raw-work' for a in artifacts): raise Refusal('no retained work archive; cleanup refused')
        for a in artifacts:
            path=safe_path(self.db.root,a['relative_path'],True)
            if digest(path.read_bytes())!=a['sha256']: raise Refusal('retention checksum mismatch; cleanup refused')
        obj=self.snapshot(id)
        if obj:
            engine.verify(obj,r)
            if obj['State']['Running'] or obj['State'].get('Restarting'): raise Refusal('live container cleanup refused')
            engine.remove(obj['Id'])
        engine.check_daemon(r['daemon_id'])
        volume=engine.volume(r['volume_name'])
        engine.check_daemon(r['daemon_id'])
        if volume:
            labels=volume.get('Labels') or {}
            if labels.get('memory-study.token')!=r['token'] or labels.get('memory-study.attempt')!=id: raise Refusal('volume ownership mismatch')
            engine.check_daemon(r['daemon_id'])
            engine.remove_volume(r['volume_name'])
        engine.check_daemon(r['daemon_id'])
        self.cleanup_receipt(id,r['volume_name'],'removed','Exact inactive attempt volume incl reproducible target; complete archive/binary hashes retained. No host target or worktree touched.',free_before,shutil.disk_usage(self.db.root).free)

    def snapshot(self,id):
        engine=self.docker(); r=self.resource(id)
        engine.check_daemon(r['daemon_id'])
        obj=engine.inspect(r['container_id'] or r['container_name'])
        engine.check_daemon(r['daemon_id'])
        if obj is not None: engine.verify(obj,r)
        return obj

    def classify(self,obj=None,error=None,cancelled=False,timedout=False,classification='infra'):
        state=obj['State'] if obj else {}
        code=state.get('ExitCode') if state.get('Status')=='exited' else None
        if state.get('OOMKilled') or code in (124,137):
            return ('failed',code,'resource-exhaustion','OOM/SIGKILL or container-owned deadline (exit 124/137)',0)
        if timedout: return ('failed',code,'resource-exhaustion','Whole-attempt deadline exceeded',0)
        if cancelled or isinstance(error,(KeyboardInterrupt,SystemExit)):
            return ('interrupted',code,'interrupted','Requested cancellation',1)
        if error:
            category='transient' if isinstance(error,TransportError) else 'fixture-parser' if isinstance(error,EngineError) else 'missing-dependency' if isinstance(error,Missing) else 'unsafe-prerequisite' if isinstance(error,Refusal) else 'fixture-parser'
            status='blocked' if category in ('missing-dependency','unsafe-prerequisite') else 'failed'
            return (status,code,category,type(error).__name__+': '+str(error)[:400],int(category=='transient'))
        if code not in (None,0):
            category='unsafe-prerequisite' if code==78 else 'missing-dependency' if code==127 else 'fixture-parser' if classification=='infra' else 'benchmark'
            return ('failed',code,category,'Isolated process exit '+str(code),0)
        return None

    def primary(self,id):
        rows=self.db.rows('SELECT o.status,o.exit_code,e.category,e.message,COALESCE(e.retryable,0) AS retryable FROM attempt_outcome o LEFT JOIN error e ON e.id=o.error_id WHERE o.attempt_id=?',(id,))
        return tuple(rows[0][k] for k in ('status','exit_code','category','message','retryable')) if rows else None

    def remember(self,id,outcome):
        if outcome is None: return
        with self.db.transaction() as c:
            if c.execute('SELECT 1 FROM attempt_outcome WHERE attempt_id=?',(id,)).fetchone(): return
            status,code,category,message,retryable=outcome
            error_id=None
            if category:
                error_id=c.execute("INSERT INTO error(attempt_id,category,message,retryable,at,role) VALUES(?,?,?,?,?,'primary')",(id,category,message,retryable,utc())).lastrowid
            c.execute('INSERT INTO attempt_outcome VALUES(?,?,?,?,?)',(id,status,code,error_id,utc()))

    def secondary(self,id,error):
        with self.db.transaction() as c:
            c.execute("INSERT INTO error(attempt_id,category,message,retryable,at,role) VALUES(?,'ownership',?,0,?,'cleanup')",(id,type(error).__name__+': '+str(error)[:400],utc()))

    def finish(self,id,status,exit_code=None,category=None,message=None,retryable=0):
        resource=self.resource(id); engine=self.docker()
        # Revalidate before AND after both absence observations, including 404.
        engine.check_daemon(resource['daemon_id'])
        absent=self.snapshot(id) is None and engine.inspect(resource['container_name']) is None
        engine.check_daemon(resource['daemon_id'])
        if not absent: raise Refusal('container absence barrier not verified; retain lease')
        if not resource['drained_at']: raise Refusal('cannot release undrained resource')
        self.remember(id,(status,exit_code,category,message,retryable))
        observed_exit=exit_code
        status,exit_code,category,message,retryable=self.primary(id)
        if exit_code is None: exit_code=observed_exit
        with self.db.transaction() as c:
            r=c.execute('SELECT * FROM resource WHERE attempt_id=?',(id,)).fetchone()
            self.active(c,id,r['token'])
            if not r['drained_at']: raise Refusal('cannot release undrained resource')
            engine.check_daemon(r['daemon_id'])
            c.execute('UPDATE attempt SET status=?,exit_code=?,ended_at=?,free_after_bytes=? WHERE id=?',(status,exit_code,utc(),shutil.disk_usage(self.db.root).free,id))
            c.execute('UPDATE trial SET status=? WHERE id=(SELECT trial_id FROM attempt WHERE id=?)',(status,id))
            c.execute('UPDATE slot SET attempt_id=NULL,token=NULL,heartbeat_at=NULL WHERE attempt_id=?',(id,))

    def attention(self,id,message):
        self.secondary(id,Refusal(message))
        with self.db.transaction() as c:
            c.execute("UPDATE attempt SET status='attention' WHERE id=?",(id,))
            c.execute("INSERT INTO recovery(attempt_id,decision,reason,at) VALUES(?,'attention',?,?)",(id,message,utc()))

    def execute(self,claim):
        id=claim['id']; engine=self.docker(); error=None; obj=None; logs=None; cancelled=False; timedout=False
        with file_lock(self.db.private/'leases'/ (id+'.lock')):
            unsafe_drain=False
            try:
                with self.db.transaction() as c:
                    self.active(c,id,claim['token'])
                    free=shutil.disk_usage(self.db.root).free
                    c.execute('UPDATE attempt SET free_before_bytes=? WHERE id=?',(free,id))
                if free<15*GIB: raise Refusal('local disk below 15GiB safety watermark')
                if driver_hash(self.db.root)!=claim['driver_sha256']: raise Refusal('code changed after claim; launch refused')
                archive=io.BytesIO()
                with tarfile.open(fileobj=archive,mode='w') as tar:
                    sources=[self.db.root/'study.py',self.db.root/'matrix.json']
                    for area,pattern in [('lib','*.py'),('recipes','*'),('migrations','*.sql')]:
                        sources += [p for p in (self.db.root/area).glob(pattern) if p.is_file()]
                    for path in sorted(sources):
                        data=path.read_bytes()
                        if len(data)>1048576: raise Refusal('oversized harness source')
                        info=tarfile.TarInfo(str(path.relative_to(self.db.root)));info.size=len(data);info.mode=0o400
                        tar.addfile(info,io.BytesIO(data))
                self.evidence(id,'harness-source','harness.tar',archive.getvalue())
                image=engine.image(claim['build']['image'])
                self.evidence(id,'input','image.json',canonical(image).encode())
                self.evidence(id,'input','load.json',canonical({'host_loadavg':os.getloadavg(),'host_load_scope':'controller host, not Docker guest','concurrent_attempts':self.db.rows('SELECT attempt_id FROM slot WHERE attempt_id IS NOT NULL'),'docker':getattr(engine,'info',{}),'python':__import__('sys').version.split()[0]}).encode())
                self.evidence(id,'input','inputs.json',canonical({'files':{k:digest(v[0]) for k,v in claim['files'].items()},'recipe':claim['recipe'],'driver_sha256':driver_hash(self.db.root)}).encode())
                r=self.resource(id)
                labels={'memory-study.token':r['token'],'memory-study.attempt':id}
                cid=engine.create(r['container_name'],r['volume_name'],claim['build']['image'],claim['command'],labels,claim['recipe'])
                obj=engine.inspect(cid)
                if obj is None: raise Refusal('created container unavailable')
                engine.verify(obj,r,claim['recipe'])
                with self.db.transaction() as c:
                    c.execute('UPDATE resource SET container_id=?,created_at=? WHERE attempt_id=?',(cid,obj['Created'],id))
                engine.upload(cid,claim['files'])
                # Provider and ELF inventory from the exact stopped image, no execution/network.
                if claim['experiment']['recipe_id']=='broker':
                    inventory=[]
                    for path in ['/usr/local/bin/dekopon-brokerd','/opt/dekopon/providers']:
                        data=engine.archive(cid,path)
                        self.evidence(id,'release-artifacts',Path(path).name+'.tar',data)
                        with tarfile.open(fileobj=io.BytesIO(data)) as tar:
                            for member in tar.getmembers():
                                if member.isfile():
                                    if member.size>64*1024*1024: raise Refusal('oversized release artifact')
                                    f=tar.extractfile(member)
                                    if f is None: raise Refusal('missing release artifact bytes')
                                    blob=f.read(64*1024*1024+1)
                                    inventory.append({'path':member.name,'bytes':len(blob),'sha256':digest(blob),'uid':member.uid,'mode':member.mode})
                    self.evidence(id,'inventory','release-inventory.json',canonical(inventory).encode())
                    with self.db.transaction() as c:
                        for item in inventory:
                            name=Path(item['path']).name
                            if name=='dekopon-brokerd':
                                prior=c.execute("SELECT binary_sha256 FROM build WHERE id='release'").fetchone()[0]
                                if prior and prior!=item['sha256']: raise Refusal('release ELF changed; new build condition required')
                                c.execute("UPDATE build SET binary_sha256=? WHERE id='release'",(item['sha256'],))
                            elif name.endswith('-provider.wasm'):
                                provider=name[:-len('-provider.wasm')]
                                old=c.execute('SELECT sha256 FROM provider WHERE id=?',(provider,)).fetchone()
                                if old is None or (old[0] and old[0]!=item['sha256']): raise Refusal('provider identity changed; new set condition required')
                                c.execute('UPDATE provider SET sha256=?,bytes=? WHERE id=?',(item['sha256'],item['bytes'],provider))
                with self.db.transaction() as c:
                    self.active(c,id,claim['token'])
                    c.execute("UPDATE attempt SET status='running' WHERE id=?",(id,))
                with self.db.transaction() as c:
                    c.execute('UPDATE execution_sequence SET launch_sequence=(SELECT COALESCE(max(launch_sequence),0)+1 FROM execution_sequence),dispatched_at=? WHERE attempt_id=?',(utc(),id))
                engine.start(cid)
                obj=engine.inspect(cid)
                if obj is None: raise Refusal('started container unavailable')
                engine.verify(obj,self.resource(id),claim['recipe'])
                with self.db.transaction() as c:
                    c.execute('UPDATE execution_sequence SET start_observed_at=? WHERE attempt_id=?',(utc(),id))
                    c.execute('UPDATE resource SET started_at=? WHERE attempt_id=?',(obj['State']['StartedAt'],id))
                    c.execute("UPDATE attempt SET status='running' WHERE id=?",(id,))
                end=time.monotonic()+claim['recipe']['timeout_s']
                while obj['State']['Running']:
                    with self.db.transaction() as c:
                        self.active(c,id,claim['token'])
                        cancelled=bool(c.execute('SELECT cancel_requested FROM attempt WHERE id=?',(id,)).fetchone()[0])
                        c.execute('UPDATE slot SET heartbeat_at=? WHERE attempt_id=?',(utc(),id))
                    timedout=time.monotonic()>=end
                    if cancelled or timedout: break
                    time.sleep(.2)
                    obj=engine.inspect(cid)
                    if obj is None: raise Refusal('running identity vanished')
                    engine.verify(obj,self.resource(id))
            except BaseException as e:
                error=e
            finally:
                # Persist the PRIMARY failure before drain can fail or replace its exit code.
                self.remember(id,self.classify(obj,error,cancelled,timedout,claim['recipe']['classification']))
                try: obj=self.drain(id)
                except BaseException as e:
                    self.attention(id,type(e).__name__+': '+str(e)[:400])
                    unsafe_drain=True
            if unsafe_drain:
                return {'attempt':id,'status':'attention','lease_held':True}
            build_kind={'build-collector':'collector','build-history':'history'}.get(claim['experiment']['recipe_id'])
            try: logs=self.retain(id,obj,build_kind)
            except BaseException as e:
                if self.primary(id): self.secondary(id,e)
                else: self.remember(id,self.classify(error=e))
                self.cleanup_receipt(id,self.resource(id)['volume_name'],'retained','Evidence collection failed: '+type(e).__name__)
            if logs is not None and not build_kind:
                try:
                    from .parse import ingest
                    ingest(self.db,id,strict=(self.primary(id) is None))
                except BaseException as e:
                    if self.primary(id): self.secondary(id,e)
                    else: self.remember(id,('failed',obj['State']['ExitCode'] if obj else None,'fixture-parser',type(e).__name__+': '+str(e)[:400],0))
            outcome=self.primary(id) or ('succeeded',obj['State']['ExitCode'] if obj else None,None,None,0)
            self.remember(id,outcome)
            status,code,category,message,retryable=outcome
            if code is None and obj is not None: code=obj['State']['ExitCode']
            try: self.remove_owned(id)
            except BaseException as e:
                self.secondary(id,e)
                self.cleanup_receipt(id,self.resource(id)['volume_name'],'refused',type(e).__name__+': '+str(e)[:300])
            try: self.finish(id,status,code,category,message,retryable)
            except BaseException as e:
                self.attention(id,type(e).__name__+': '+str(e)[:400])
                return {'attempt':id,'status':'attention','lease_held':True}
            return {'attempt':id,'cell':claim['experiment']['id'],'status':status,'exit_code':code,'error':category}

    def recover(self,id):
        try:
            with file_lock(self.db.private/'controller.lock',exclusive=False), file_lock(self.db.private/'leases'/(id+'.lock')):
                if not self.db.rows('SELECT 1 FROM slot WHERE attempt_id=?',(id,)): raise Refusal('attempt has no active slot')
                try:
                    observed=self.snapshot(id)
                    classification=self.db.rows('SELECT r.classification FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id JOIN recipe r ON r.id=e.recipe_id WHERE a.id=?',(id,))[0]['classification']
                    if not self.primary(id):
                        previous=self.db.rows("SELECT category,message,retryable FROM error WHERE attempt_id=? AND role='primary' AND category!='ownership' ORDER BY id LIMIT 1",(id,))
                        if previous:
                            e=previous[0]; outcome=('interrupted' if e['category']=='interrupted' else 'failed',None,e['category'],e['message'],e['retryable'])
                        else:
                            outcome=self.classify(observed,classification=classification) or ('interrupted',None,'interrupted','Owner lost; completed success/parse not established',1)
                        self.remember(id,outcome)
                    obj=self.drain(id)
                except BaseException as e:
                    self.attention(id,type(e).__name__+': '+str(e)[:400]); return {'attempt':id,'status':'attention','lease_held':True}
                # Preserve all synthetic work before the final deletion barrier; no overwrite.
                if obj is not None:
                    try:
                        self.retain(id,obj)
                        self.remove_owned(id)
                    except BaseException as e:
                        self.attention(id,type(e).__name__+': '+str(e)[:400])
                        return {'attempt':id,'status':'attention','lease_held':True}
                else:
                    self.cleanup_receipt(id,self.resource(id)['volume_name'],'retained','No container exists; any partial volume retained for owner salvage.')
                outcome=self.primary(id)
                final=list(outcome)
                if final[1] is None and obj is not None: final[1]=obj['State']['ExitCode']
                try: self.finish(id,*final)
                except BaseException as e:
                    self.attention(id,str(e)); return {'attempt':id,'status':'attention','lease_held':True}
                with self.db.transaction() as c: c.execute("INSERT INTO recovery(attempt_id,decision,reason,at) VALUES(?,'drained',?,?)",(id,'Owner lock acquired; same daemon/container/create/start identity verified',utc()))
                return {'attempt':id,'status':outcome[0],'lease_held':False}
        except Refusal as e:
            with self.db.transaction() as c: c.execute("INSERT INTO recovery(attempt_id,decision,reason,at) VALUES(?,'refused',?,?)",(id,str(e),utc()))
            raise

    def run(self,campaign,lane,max_cells=6):
        if not 1<=max_cells<=6: raise Refusal('worker bound is 1..6 cells')
        results=[]; cells=set()
        with file_lock(self.db.private/'controller.lock',exclusive=False):
            # At most 3 replicates per cell, no automatic retries. Independently blocked work passes.
            for _ in range(18):
                if len(cells)>=max_cells:
                    pending=self.db.rows("SELECT DISTINCT experiment_id FROM trial WHERE campaign_id=? AND status='pending'",(campaign,))
                    if any(r['experiment_id'] not in cells for r in pending): break
                claim=self.claim(campaign,lane)
                if claim is None: break
                cells.add(claim['experiment']['id'])
                results.append(self.execute(claim))
                if results[-1]['status']=='attention': break
        return {'campaign':campaign,'lane':lane,'results':results,'database_is_authoritative':True}
