"""Append-only parser versions; raw log hashes are the analysis authority."""
import json
import math
import statistics
import uuid
from .ledger import Refusal, digest, safe_path, utc, canonical

def percentile(values, p):
    # Nearest rank, no fabricated interpolation/sample count.
    values=sorted(values)
    return values[max(0,math.ceil(len(values)*p)-1)]

def ingest(db,id,strict=True):
    with db.transaction() as c:
        a=c.execute("SELECT * FROM artifact WHERE attempt_id=? AND kind='raw-log'",(id,)).fetchone()
        if a is None: raise Refusal('no retained raw log')
        data=safe_path(db.root,a['relative_path'],True).read_bytes()
        if digest(data)!=a['sha256']: raise Refusal('raw checksum mismatch')
        if len(data)>8388608: raise Refusal('raw log cap exceeded')
        sha=digest((db.root/'lib/parse.py').read_bytes())
        last=c.execute('SELECT * FROM latest_parse WHERE attempt_id=?',(id,)).fetchone()
        if last and last['parser_sha256']==sha and last['input_artifact_id']==a['id']:
            return {'parse_id':last['id'],'created':False}
        parse_id=uuid.uuid4().hex
        c.execute('INSERT INTO parse_run VALUES(?,?,?,?,?,?)',(parse_id,id,sha,a['id'],utc(),last['id'] if last else None))
        known={r[0] for r in c.execute('SELECT id FROM metric')}; observed=set(); groups={}; env=None; completed=False
        history=c.execute("SELECT 1 FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id WHERE a.id=? AND e.recipe_id IN ('history','history-smoke')",(id,)).fetchone() is not None
        events=[]
        for ordinal,line in enumerate(data.splitlines()):
            if ordinal>=20000 or len(line)>65536: raise Refusal('bounded parser line/count ceiling')
            if line.startswith(b'{'): events.append((ordinal,json.loads(line)))
        phases=['load','retained','clones','clones-dropped','dropped','throughput']
        markers=[o for _,o in events if o.get('kind')=='phase-sync']
        synced=history and [o.get('phase') for o in markers]==phases and all(o.get('protocol')=='history-pipe-v1' for o in markers)
        logical={'retained_text_bytes','held_seed_text_bytes','history_turn_count','operation_latency_ns','clone_latency_ns','operations_per_second'}
        memory={'process_rss_bytes','process_pss_bytes','cgroup_memory_current_bytes'}
        transitions=[o.get('phase') for _,o in events if o.get('kind')=='phase-transition']
        synced=synced and transitions==['clones-dropped','dropped']
        if synced:
            # Every phase has an acknowledged, observer-clock deep snapshot. No inferred
            # clock subtraction or counter-only phase can qualify as memory evidence.
            current='startup'; snapshots=set(); last=-1
            for _,o in events:
                if o.get('kind')=='phase-transition': current='transition'
                elif o.get('kind')=='phase-sync':
                    if not isinstance(o.get('elapsed_ns'),int) or o['elapsed_ns']<=last: raise Refusal('invalid phase synchronization clock')
                    current=o['phase']; last=o['elapsed_ns']
                elif o.get('clock_origin')=='observer-relative' and memory<=set(o.get('metrics',{})):
                    if o['phase']!=current or o['elapsed_ns']<last: raise Refusal('memory snapshot outside synchronized phase')
                    snapshots.add(current)
            synced=set(phases)<=snapshots
        if strict and history and not synced: raise Refusal('History phase-specific memory reconstruction unverified')
        scope='history-synchronized' if synced else 'whole-run-only' if history else 'recipe-phases'
        c.execute('INSERT INTO parse_scope VALUES(?,?,?)',(parse_id,scope,'history-pipe-v1' if synced else 'legacy-unaligned' if history else 'observer-relative'))
        for ordinal,obj in events:
            if obj.get('kind')=='environment': env=obj; continue
            if obj.get('kind')=='complete': completed=obj.get('exit_code')==0; continue
            if 'metrics' not in obj: continue
            phase=obj['phase']; elapsed=obj['elapsed_ns']
            if not isinstance(phase,str) or len(phase)>40 or not isinstance(elapsed,int) or elapsed<0: raise Refusal('invalid phase/clock')
            if not isinstance(obj['metrics'],dict) or len(obj['metrics'])>40: raise Refusal('invalid metric map')
            for metric,value in obj['metrics'].items():
                if metric not in known or isinstance(value,bool) or not isinstance(value,(int,float)) or not math.isfinite(value) or value<0: raise Refusal('invalid metric/unit/value')
                sample_phase='whole-run' if history and not synced and metric not in logical else phase
                clock=obj.get('clock_origin','child-relative' if history and metric in logical else 'legacy-unspecified')
                if clock not in ('observer-relative','child-relative','legacy-unspecified'): raise Refusal('unknown sample clock')
                c.execute('INSERT INTO sample VALUES(?,?,?,?,?,?,?)',(parse_id,ordinal,sample_phase,elapsed,metric,value,clock))
                observed.add(metric); groups.setdefault((sample_phase,metric),[]).append(value)
        if strict and (not completed or not env or not groups): raise Refusal('missing completion/environment/measurement protocol')
        if env:
            if env.get('memory_max')!=1073741824 or env.get('memory_swap_max')!=0 or env.get('pids_max')!=64 or env.get('cpu_quota')!=env.get('cpu_period') or env.get('docker_disk_free_bytes',0)<8*1024**3: raise Refusal('unverified in-container resource/disk limits')
            resource=c.execute('SELECT daemon_id FROM resource WHERE attempt_id=?',(id,)).fetchone()
            eid='environment-'+id
            c.execute('INSERT OR IGNORE INTO environment VALUES(?,?,?,?,?,?,?,?,?,?)',(eid,env['os'],env['architecture'],env['page_bytes'],env['collector_libc'],resource['daemon_id'],canonical(env),'{}','Docker private disk-backed volume (not host /tmp)',canonical({'collector':'static-v1','parser':sha})))
            c.execute('UPDATE attempt SET environment_id=? WHERE id=?',(eid,id))
        for (phase,metric),values in groups.items():
            c.execute('INSERT INTO phase_summary VALUES(?,?,?,?,?,?,?,?,?)',(parse_id,phase,metric,len(values),min(values),max(values),statistics.fmean(values),percentile(values,.5),percentile(values,.95)))
        for metric in observed:
            c.execute('DELETE FROM missing_metric WHERE attempt_id=? AND metric_id=?',(id,metric))
        for metric in known-observed:
            c.execute('INSERT OR IGNORE INTO missing_metric VALUES(?,?,?)',(id,metric,'Not exposed by this recipe/instrumentation or process ended before capture; never zero.'))
        parser_path='private/raw/'+id+'/parser-'+sha+'.py'
        if not c.execute('SELECT 1 FROM artifact WHERE relative_path=?',(parser_path,)).fetchone():
            db.artifact(c,id,'parser-source',parser_path,(db.root/'lib/parse.py').read_bytes())
        return {'parse_id':parse_id,'created':True,'measurements':sum(map(len,groups.values()))}
