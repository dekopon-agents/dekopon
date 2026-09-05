"""Canonical SQLite ledger. No production paths, SQL shell, or alternate CLI database."""
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import random
import sqlite3
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
GIB = 1024 ** 3
PARSER = 'jsonl-v1'

class Refusal(RuntimeError):
    pass

def utc():
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())

def digest(data):
    return hashlib.sha256(data).hexdigest()

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False)

def driver_hash(root=ROOT):
    paths = [root / 'study.py', root / 'matrix.json']
    for area, glob in [('lib', '*.py'), ('recipes', '*'), ('migrations', '*.sql')]:
        paths += [p for p in (root / area).glob(glob) if p.is_file()]
    return digest(b''.join(str(p.relative_to(root)).encode()+b'\0'+p.read_bytes()+b'\0' for p in sorted(paths)))

def safe_path(root, relative, must_exist=False):
    relative = Path(relative)
    if relative.is_absolute() or '..' in relative.parts or not relative.parts:
        raise Refusal('out-of-root path')
    current = root.resolve()
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise Refusal('symlink path refused')
    if not current.is_relative_to(root.resolve()):
        raise Refusal('out-of-root path')
    if must_exist and not current.exists():
        raise Refusal('missing owned path')
    return current

@contextlib.contextmanager
def file_lock(path, exclusive=True, nonblocking=True):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.is_symlink() or any(p.is_symlink() for p in path.parents):
        raise Refusal('symlink lock')
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(fd, (fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH) | (fcntl.LOCK_NB if nonblocking else 0))
        except BlockingIOError as e:
            raise Refusal('controller/attempt is active; lock refused') from e
        yield
    finally:
        os.close(fd)

class Ledger:
    def __init__(self, root=ROOT):
        # A root override exists only for isolated unit tests; CLI never exposes one.
        self.root = root.resolve()
        self.private = safe_path(self.root, 'private')
        self.path = safe_path(self.root, 'private/ledger.sqlite3')

    def connect(self, verify=True):
        if not self.path.exists():
            raise Refusal('run init first')
        conn = sqlite3.connect(self.path, timeout=10, isolation_level=None)
        conn.row_factory = sqlite3.Row
        conn.execute('PRAGMA foreign_keys=ON')
        conn.execute('PRAGMA busy_timeout=10000')
        conn.execute('PRAGMA synchronous=FULL')
        if verify:
            current=conn.execute('PRAGMA user_version').fetchone()[0]
            for version,sql in self.migrations():
                if version<=current:
                    row=conn.execute('SELECT sha256 FROM schema_migrations WHERE version=?',(version,)).fetchone()
                    if not row or row[0]!=digest(sql.encode()):
                        conn.close(); raise Refusal('applied migration hash changed; never rewrite old migrations')
            identity = conn.execute('SELECT * FROM executor_identity').fetchone()
            marker = safe_path(self.root, 'private/identity.json', True)
            st = self.path.stat()
            if not identity or json.loads(marker.read_text()) != {'uuid': identity['uuid'], 'device': st.st_dev, 'inode': st.st_ino} or (identity['db_device'], identity['db_inode']) != (st.st_dev, st.st_ino):
                conn.close()
                raise Refusal('canonical database identity changed; offline relocation required')
        return conn

    @contextlib.contextmanager
    def transaction(self):
        c = self.connect()
        try:
            c.execute('BEGIN IMMEDIATE')
            yield c
            c.execute('COMMIT')
        except BaseException:
            if c.in_transaction:
                c.execute('ROLLBACK')
            raise
        finally:
            c.close()

    def migrations(self):
        return [(int(p.stem),p.read_text()) for p in sorted((self.root/'migrations').glob('*.sql'))]

    def migrate(self):
        with file_lock(self.private/'controller.lock'):
            c=self.connect()
            try:
                self.quiescent(c)
                current=c.execute('PRAGMA user_version').fetchone()[0]
                pending=[(v,sql) for v,sql in self.migrations() if v>current]
                if not pending: return {'applied':[]}
                script='BEGIN IMMEDIATE;\n'
                for v,sql in pending:
                    script+=sql+'\nINSERT INTO schema_migrations VALUES('+str(v)+',\''+digest(sql.encode())+'\');\n'
                c.executescript(script)
                c.commit()
                return {'applied':[v for v,sql in pending]}
            except BaseException:
                if c.in_transaction: c.rollback()
                raise
            finally: c.close()

    def init(self):
        self.private.mkdir(mode=0o700, exist_ok=True)
        if self.private.stat().st_mode & 0o077:
            raise Refusal('private directory must be owner-only')
        with file_lock(self.private / 'controller.lock'):
            if self.path.exists():
                with contextlib.closing(self.connect()) as c:
                    if c.execute('PRAGMA user_version').fetchone()[0] != self.migrations()[-1][0]:
                        raise Refusal('pending schema migration; run migrate')
                return {'initialized': False}
            c = sqlite3.connect(self.path)
            try:
                c.execute('PRAGMA foreign_keys=ON')
                c.execute('PRAGMA journal_mode=WAL')
                migrations=self.migrations()
                c.executescript('BEGIN IMMEDIATE;\n' + '\n'.join(sql for v,sql in migrations))
                for v,sql in migrations: c.execute('INSERT INTO schema_migrations VALUES(?,?)', (v,digest(sql.encode())))
                st = self.path.stat()
                id = uuid.uuid4().hex
                c.execute('INSERT INTO executor_identity VALUES(1,?,?,?,?)', (id,driver_hash(self.root),st.st_dev,st.st_ino))
                self.seed(c)
                c.commit()
                marker = self.private / 'identity.json'
                marker.write_text(canonical({'uuid':id,'device':st.st_dev,'inode':st.st_ino})+'\n')
                marker.chmod(0o600)
                self.path.chmod(0o600)
            finally:
                c.close()
        return {'initialized': True}

    def seed(self, c):
        spec = json.loads((self.root / 'matrix.json').read_text())
        c.execute('INSERT INTO study VALUES(?,?,?)', ('memory','Dekopon bounded memory study',digest((self.root/'DESIGN_DIGEST.md').read_bytes())))
        for id,sha,version in [('R','b9b9533a9050a5bfe5096bffc0de40ee1ffd8f42','0.12.0'),('M','5a03a296ff4877cef4549ad83db46e52f6cbe231','post-0.12.0')]:
            c.execute('INSERT INTO source_revision VALUES(?,?,NULL,?,?)',(id,sha,version,'Owner-established source; no production inspection.'))
        for b in spec['builds']:
            c.execute('INSERT INTO build(id,source_id,image,compiler,flags,allocator) VALUES(?,?,?,?,?,?)',tuple(b[k] for k in ['id','source','image','compiler','flags','allocator']))
        for h in spec['hypotheses']:
            c.execute('INSERT INTO subsystem VALUES(?,?,?)',(h['subsystem'],h['lane'],h['subsystem']))
            c.execute('INSERT INTO hypothesis VALUES(?,?,?,?,?)',(h['id'],h['subsystem'],h['question'],h['disposition'],h['limitation']))
            ref = h['id']+'-source'
            rev,loc = h['reference'].split(':',1)
            c.execute('INSERT INTO reference VALUES(?,?,?,?)',(ref,rev,('crates/'+loc if loc.startswith('dekopond/') else 'crates/dekopon-'+loc),'Mapper source evidence; full qualifiers in DESIGN_DIGEST.md'))
            c.execute('INSERT INTO hypothesis_reference VALUES(?,?)',(h['id'],ref))
            if h['disposition']=='deferred':
                c.execute('INSERT INTO followup VALUES(?,?,?,?,?,?,?)',(h['id'],h['id'],h['question'],'Resolve source-supported attribution gap',h['limitation'],'one validated cell;3 replicates;180s each','blocked'))
        for id in ['echo','http-probe','jsonplaceholder','gh']:
            c.execute('INSERT INTO provider(id) VALUES(?)',(id,))
        for id,providers in [('P4',['echo','gh','http-probe','jsonplaceholder']),('echo',['echo'])]:
            c.execute('INSERT INTO provider_set VALUES(?,?)',(id,'Representative baked set, not deployed parity; inventory hashes required per attempt.'))
            for i,p in enumerate(providers):
                c.execute('INSERT INTO provider_member VALUES(?,?,?)',(id,i,p))
        for r in spec['recipes']:
            c.execute('INSERT INTO recipe VALUES(?,?,?,?,?,?,?,?)',tuple(r[k] for k in ['id','implementation','classification','timeout_s','memory_bytes','pids','cpus','output_bytes']))
        for p in ['collector','history']:
            c.execute('INSERT INTO prerequisite VALUES(?,?,?)',(p,'Successful, hash-verified study '+p+' static build','artifact'))
        for e in spec['experiments']:
            config = e['factors']
            c.execute('INSERT INTO experiment VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',(e['id'],'memory',e['hypothesis'],e['lane'],e['stage'],e['priority'],e['recipe'],e['build'],e['providers'],e['status'],e['reason'],canonical(config)))
            for k,v in config.items():
                kind = 'boolean' if isinstance(v,bool) else 'integer' if isinstance(v,int) else 'real' if isinstance(v,float) else 'text'
                unit = 'byte' if 'bytes' in k else 'millisecond' if k.endswith('_ms') else 'second' if k.endswith('_seconds') else 'category' if kind=='text' else 'count'
                level = canonical(v)
                c.execute('INSERT OR IGNORE INTO factor VALUES(?,?,?)',(k,kind,unit))
                c.execute('INSERT OR IGNORE INTO factor_level VALUES(?,?,?,?,?,?)',(k,level,kind,int(v) if kind in ('integer','boolean') else None,v if kind=='real' else None,v if kind=='text' else None))
                c.execute('INSERT INTO assignment VALUES(?,?,?)',(e['id'],k,level))
            for p in e['prerequisites']:
                c.execute('INSERT INTO experiment_prerequisite VALUES(?,?)',(e['id'],p))
            if e['status']=='unsupported':
                p=e['id']+'-driver'
                c.execute('INSERT INTO prerequisite VALUES(?,?,?)',(p,e['reason'],'implemented'))
                c.execute('INSERT INTO experiment_prerequisite VALUES(?,?)',(e['id'],p))
        from .recipes import register_versions
        register_versions(self,c)
        c.execute('INSERT INTO experiment_dependency VALUES(?,?)',('RT-C04','RT-C03'))
        self.followup_dependencies(c)
        for id,q,members in [('runtime-screen','In-container observer provider-set sensitivity',[('RT-X01','baseline'),('RT-X02','variant')]),('external-screen','External observer release screen',[(f'RT-C{i:02}','baseline' if i==1 else 'control' if i==5 else 'variant') for i in range(1,7)])]:
            c.execute('INSERT INTO comparison_group VALUES(?,?,3)',(id,q))
            c.executemany('INSERT INTO comparison_member VALUES(?,?,?)',[(id,*x) for x in members])
        metrics = {
            'process_rss_bytes':('byte','Proc status VmRSS: child process resident pages, not cgroup ownership','gauge'),
            'process_pss_bytes':('byte','smaps_rollup Pss; proportionally shared process pages','gauge'),
            'anonymous_rss_bytes':('byte','smaps_rollup Anonymous, not live malloc bytes','gauge'),
            'anonymous_executable_rss_bytes':('byte','smaps anonymous unnamed executable mapping RSS, JIT-consistent only','gauge'),
            'process_virtual_bytes':('byte','VmSize; virtual reservations are not resident memory','gauge'),
            'process_hwm_bytes':('byte','VmHWM kernel process high-water RSS','gauge'),
            'collector_rss_bytes':('byte','Observer process RSS, separately measured; do not subtract unlike accounting','gauge'),
            'cgroup_memory_current_bytes':('byte','memory.current entire experiment tree incl collector','gauge'),
            'cgroup_memory_peak_bytes':('byte','memory.peak kernel charge high-water entire tree','gauge'),
            'cgroup_working_set_bytes':('byte','max(0,memory.current-inactive_file); not process RSS','gauge'),
            'cgroup_anon_bytes':('byte','memory.stat anon','gauge'),
            'cgroup_file_bytes':('byte','memory.stat file incl cache','gauge'),
            'cgroup_kernel_bytes':('byte','memory.stat kernel','gauge'),
            'cpu_time_ns':('nanosecond','wait4 child user+system CPU, descendants if waited; not sampler CPU','counter'),
            'wall_time_ns':('nanosecond','Monotonic since observer start, includes startup','duration'),
            'ready_latency_ns':('nanosecond','First observed synthetic broker socket; sampling-delayed readiness proxy','duration'),
            'minor_faults':('count','wait4 child minor faults','counter'),
            'major_faults':('count','wait4 child major faults','counter'),
            'read_bytes':('byte','Proc io read_bytes; actual block input, not logical read length','counter'),
            'write_bytes':('byte','Proc io write_bytes; actual block output, not logical file size','counter'),
            'retained_text_bytes':('byte','History::bytes sum; excludes allocator/container capacities','gauge'),
            'held_seed_text_bytes':('byte','Text bytes in four held History clones; not a gateway seed','gauge'),
            'history_turn_count':('count','Sum retained History turns','gauge'),
            'operation_latency_ns':('nanosecond','History::record duration excludes fixture construction','duration'),
            'clone_latency_ns':('nanosecond','History::clone duration','duration'),
            'operations_per_second':('count/second','Completed record operations / measured record time only','gauge'),
            'mallinfo2_uordblks_bytes':('byte','glibc allocated-block accounting; unavailable without matched observer','gauge'),
        }
        for id,(unit,definition,agg) in metrics.items(): c.execute('INSERT INTO metric VALUES(?,?,?,?)',(id,unit,definition,agg))
        for id,owner,scope,unit,ceiling,response,tradeoff in [
            ('aggregate-guest','broker-host','all guest memories across stores','byte','hard','reject admission','Multiply memory count; native/JIT/stacks require independent headroom.'),
            ('history-budget','gateway','all retained histories and active clones','byte','hard','evict history or reject work','Never cache authorization; serialized request copies separate.'),
            ('allocator-trim','native allocator','free retained pages','byte','soft','trim','Linux-specific; CPU, contention and next-request faults.'),
            ('replay-disk','broker','permanent exact replay index and WAL','byte','hard','fail closed','Exactness, watermark recovery, latency/I/O; no TTL.'),
            ('overlay-budget','storage-host','all native overlays','byte','hard','reject or reviewed spill','Disk quota, crash consistency and namespace isolation.'),
            ('queue-budget','gateway/telemetry','per queue count/bytes/age/inflight','byte','hard','reject replies; drop telemetry only','Busy replies outside session permits; audit never lossy.')]:
            c.execute('INSERT INTO proposed_control VALUES(?,?,?,?,?,?,?,?)',(id,owner,scope,unit,ceiling,response,'Exploration',tradeoff))
        c.execute('INSERT INTO finding VALUES(?,?,?,?)',('F-001','source-supported','A per-store maxMemoryBytes reservation does not multiply by allowed memory count.','Unexecuted two-memory fixture; not a production bypass claim.'))
        c.execute('INSERT INTO finding_evidence(finding_id,reference_id) VALUES(?,?)',('F-001','RT-H08-source'))

    def quiescent(self, c):
        if c.execute('SELECT 1 FROM slot WHERE attempt_id IS NOT NULL').fetchone():
            raise Refusal('active or unrecovered slots; controller boundary refused')

    def followup_dependencies(self,c):
        for h,e in [('RT-H04','RT-C01'),('RT-H06','RT-C05'),('RT-H07','RT-C02'),('RT-H08','RT-D07'),('RT-H09','RT-C02'),('H-STATE-007','C-STATE-003'),('H-STATE-008','C-STATE-002'),('H-STATE-009','C-STATE-005')]:
            c.execute('INSERT OR IGNORE INTO followup_dependency VALUES(?,?)',(h,e))

    def add_conditions(self):
        """Add-only bootstrap/continuation matrix registration; existing conditions are immutable."""
        spec=json.loads((self.root/'matrix.json').read_text())
        with file_lock(self.private/'controller.lock'), self.transaction() as c:
            self.quiescent(c)
            added=[]
            for b in spec['builds']:
                values=tuple(b[k] for k in ['id','source','image','compiler','flags','allocator'])
                old=c.execute('SELECT id,source_id,image,compiler,flags,allocator FROM build WHERE id=?',(b['id'],)).fetchone()
                if old and tuple(old)!=values: raise Refusal('changed build needs a new ID')
                if not old: c.execute('INSERT INTO build(id,source_id,image,compiler,flags,allocator) VALUES(?,?,?,?,?,?)',values)
            for r in spec['recipes']:
                values=tuple(r[k] for k in ['id','implementation','classification','timeout_s','memory_bytes','pids','cpus','output_bytes'])
                old=c.execute('SELECT * FROM recipe WHERE id=?',(r['id'],)).fetchone()
                if old and tuple(old)!=values: raise Refusal('changed recipe needs a new ID')
                if not old: c.execute('INSERT INTO recipe VALUES(?,?,?,?,?,?,?,?)',values)
            for e in spec['experiments']:
                values=(e['id'],'memory',e['hypothesis'],e['lane'],e['stage'],e['priority'],e['recipe'],e['build'],e['providers'],e['status'],e['reason'],canonical(e['factors']))
                old=c.execute('SELECT * FROM experiment WHERE id=?',(e['id'],)).fetchone()
                if old and (tuple(old)[:9]+(old['config_json'],))!=(values[:9]+(values[-1],)): raise Refusal('changed experiment needs a new ID')
                if old: continue
                # New source variants are bounded to existing recipes and prerequisites.
                c.execute('INSERT INTO experiment VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',values)
                for k,v in e['factors'].items():
                    kind='boolean' if isinstance(v,bool) else 'integer' if isinstance(v,int) else 'real' if isinstance(v,float) else 'text'
                    unit='byte' if 'bytes' in k else 'millisecond' if k.endswith('_ms') else 'second' if k.endswith('_seconds') else 'category' if kind=='text' else 'count'
                    c.execute('INSERT OR IGNORE INTO factor VALUES(?,?,?)',(k,kind,unit))
                    c.execute('INSERT OR IGNORE INTO factor_level VALUES(?,?,?,?,?,?)',(k,canonical(v),kind,int(v) if kind in ('integer','boolean') else None,v if kind=='real' else None,v if kind=='text' else None))
                    c.execute('INSERT INTO assignment VALUES(?,?,?)',(e['id'],k,canonical(v)))
                for p in e['prerequisites']: c.execute('INSERT INTO experiment_prerequisite VALUES(?,?)',(e['id'],p))
                added.append(e['id'])
            self.followup_dependencies(c)
            expected_order=['echo','gh','http-probe','jsonplaceholder']
            old_order=[r[0] for r in c.execute("SELECT provider_id FROM provider_member WHERE set_id='P4' ORDER BY ordinal")]
            if old_order!=expected_order:
                if c.execute("SELECT 1 FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id WHERE e.lane='runtime'").fetchone(): raise Refusal('provider order correction after runtime trials requires new set ID')
                self.artifact(c,None,'reference-correction','private/controller/'+uuid.uuid4().hex+'.json',canonical({'provider_set':'P4','old_order':old_order,'filename_order':expected_order}).encode())
                c.execute("DELETE FROM provider_member WHERE set_id='P4'")
                c.executemany('INSERT INTO provider_member VALUES(?,?,?)',[('P4',i,p) for i,p in enumerate(expected_order)])
            for h in spec['hypotheses']:
                rev,loc=h['reference'].split(':',1)
                expected='crates/'+loc if loc.startswith('dekopond/') else 'crates/dekopon-'+loc
                old=c.execute('SELECT locator FROM reference WHERE id=?',(h['id']+'-source',)).fetchone()
                if old and old[0]!=expected:
                    self.artifact(c,None,'reference-correction','private/controller/'+uuid.uuid4().hex+'.json',canonical({'id':h['id']+'-source','previous':old[0],'corrected':expected}).encode())
                    c.execute('UPDATE reference SET locator=? WHERE id=?',(expected,h['id']+'-source'))
            return {'added':added}

    def sync_driver(self):
        with file_lock(self.private/'controller.lock'), self.transaction() as c:
            self.quiescent(c)
            if c.execute('PRAGMA user_version').fetchone()[0]!=self.migrations()[-1][0]: raise Refusal('pending schema migration')
            from .recipes import register_versions
            register_versions(self,c)
            c.execute('UPDATE executor_identity SET driver_sha256=?',(driver_hash(self.root),))
        return {'driver_sha256':driver_hash(self.root)}

    def campaign(self, id, seed, cells, replicates):
        if not id or len(id)>64 or any(ch not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for ch in id):
            raise Refusal('campaign id must be a short portable identifier')
        if replicates not in [1,2,3]: raise Refusal('replicates must be 1..3')
        with file_lock(self.private/'controller.lock'), self.transaction() as c:
            self.quiescent(c)
            if c.execute('PRAGMA user_version').fetchone()[0]!=self.migrations()[-1][0]: raise Refusal('pending schema migration')
            dh=driver_hash(self.root)
            if dh!=c.execute('SELECT driver_sha256 FROM executor_identity').fetchone()[0]: raise Refusal('driver changed: controller-sync at quiescent boundary')
            existing=c.execute('SELECT * FROM campaign WHERE id=?',(id,)).fetchone()
            if existing:
                actual={(r['experiment_id'],r['replicate']) for r in c.execute('SELECT * FROM trial WHERE campaign_id=?',(id,))}
                if existing['seed']!=seed or existing['driver_sha256']!=dh or actual!={(e,r) for e in cells for r in range(1,replicates+1)}:
                    raise Refusal('campaign ID already binds different seed/driver/matrix; use new ID')
                return {'campaign':id,'created':False}
            known={r[0] for r in c.execute('SELECT id FROM experiment')}
            if not cells or len(cells)>32 or len(set(cells))!=len(cells) or not set(cells)<=known: raise Refusal('unknown/duplicate/empty/oversized cell selection')
            c.execute('INSERT INTO campaign VALUES(?,?,?,?,?)',(id,utc(),seed,dh,'open'))
            order=[(e,r) for r in range(1,replicates+1) for e in cells]
            random.Random(seed).shuffle(order)
            # Claim-time dependency checks preserve the shuffled order among ready trials.
            for i,(e,r) in enumerate(order):
                status='pending' if c.execute('SELECT status FROM experiment WHERE id=?',(e,)).fetchone()[0]=='ready' else 'blocked'
                pair=f'{id}:RT-C03:r{r}' if e=='RT-C04' and 'RT-C03' in cells else None
                c.execute('INSERT INTO trial VALUES(?,?,?,?,?,?,?)',(f'{id}:{e}:r{r}',id,e,r,i,pair,status))
        return {'campaign':id,'created':True,'trials':len(order),'comparative':replicates==3}

    def artifact(self, c, attempt_id, kind, relative, content):
        path=safe_path(self.root,relative)
        path.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
        with path.open('xb') as f:
            f.write(content); f.flush(); os.fsync(f.fileno())
        path.chmod(0o400)
        id=uuid.uuid4().hex
        c.execute('INSERT INTO artifact VALUES(?,?,?,?,?,?,?,?)',(id,attempt_id,kind,relative,digest(content),len(content),'private',utc()))
        return id

    def rows(self, query, params=()):
        c=self.connect()
        try: return [dict(r) for r in c.execute(query,params)]
        finally: c.close()
