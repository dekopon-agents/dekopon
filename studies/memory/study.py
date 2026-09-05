#!/usr/bin/env python3
"""All shared study mutations go through this CLI; never accepts --db or arbitrary commands."""
import argparse
import json
import os
import re
import sqlite3
import sys
from lib.ledger import Ledger, Refusal, canonical, driver_hash, file_lock, utc
from lib.executor import Executor

def main():
    os.umask(0o077)
    parser=argparse.ArgumentParser(description=__doc__)
    commands=parser.add_subparsers(dest='action',required=True)
    for name in ['init','controller-sync','add-conditions','migrate','check']: commands.add_parser(name)
    status=commands.add_parser('status'); status.add_argument('--campaign'); status.add_argument('--lane',choices=['runtime','state','infra'])
    plan=commands.add_parser('plan'); plan.add_argument('--campaign',required=True); plan.add_argument('--seed',type=int,default=20260905); plan.add_argument('--cells',nargs='+',required=True); plan.add_argument('--replicates',type=int,choices=[1,2,3],default=3)
    run=commands.add_parser('run'); run.add_argument('--campaign',required=True); run.add_argument('--lane',required=True,choices=['runtime','state','infra']); run.add_argument('--max-cells',type=int,default=6)
    for name in ['cancel','recover','retry','cleanup','reparse']:
        sub=commands.add_parser(name); sub.add_argument('--attempt',required=True)
    disable=commands.add_parser('disable'); disable.add_argument('--cell',required=True); disable.add_argument('--reason',choices=['replaced-condition','missing-artifact','unsafe-prerequisite','driver-gap'],required=True)
    cont=commands.add_parser('continue'); cont.add_argument('--campaign',required=True)
    finding=commands.add_parser('record-findings'); finding.add_argument('--file',default='findings.json')
    ex=commands.add_parser('export'); ex.add_argument('--output',default='exports/snapshot.sql')
    back=commands.add_parser('backup'); back.add_argument('--name',required=True)
    restore=commands.add_parser('restore'); restore.add_argument('--name',required=True)
    args=parser.parse_args(); db=Ledger()
    if hasattr(args,'attempt') and not re.fullmatch(r'[0-9a-f]{32}',args.attempt): raise Refusal('invalid attempt identity')
    if args.action=='init': return db.init()
    if args.action=='controller-sync': return db.sync_driver()
    if args.action=='add-conditions': return db.add_conditions()
    if args.action=='migrate': return db.migrate()
    if args.action=='plan': return db.campaign(args.campaign,args.seed,args.cells,args.replicates)
    if args.action=='check':
        c=db.connect()
        try:
            integrity=c.execute('PRAGMA integrity_check').fetchone()[0]
            fks=[tuple(r) for r in c.execute('PRAGMA foreign_key_check')]
            schema=c.execute('PRAGMA user_version').fetchone()[0]
            current=driver_hash(db.root)==c.execute('SELECT driver_sha256 FROM executor_identity').fetchone()[0]
            from lib.ledger import digest
            migration=all((row:=c.execute('SELECT sha256 FROM schema_migrations WHERE version=?',(v,)).fetchone()) is not None and row[0]==digest(sql.encode()) for v,sql in db.migrations()) and schema==db.migrations()[-1][0]
            if integrity!='ok' or fks or not migration or not current: raise Refusal('integrity/schema/driver identity failed')
            return {'integrity':integrity,'foreign_key_failures':fks,'schema_version':schema,'driver_current':current,'leases':db.rows('SELECT * FROM slot')}
        finally: c.close()
    if args.action=='disable':
        with file_lock(db.private/'controller.lock'), db.transaction() as c:
            db.quiescent(c)
            old=c.execute('SELECT * FROM experiment WHERE id=?',(args.cell,)).fetchone()
            if not old: raise Refusal('unknown cell')
            import uuid
            db.artifact(c,None,'controller-decision','private/controller/'+uuid.uuid4().hex+'.json',canonical({'cell':args.cell,'old_status':old['status'],'reason':args.reason,'at':utc()}).encode())
            c.execute("UPDATE experiment SET status='blocked',reason=? WHERE id=?",(args.reason,args.cell))
            c.execute("UPDATE trial SET status='blocked' WHERE experiment_id=? AND status='pending'",(args.cell,))
        return {'cell':args.cell,'status':'blocked','reason':args.reason}
    if args.action=='status':
        result={'coverage':db.rows('SELECT * FROM coverage'+(' WHERE lane=?' if args.lane else '')+' ORDER BY lane,stage,id',(args.lane,) if args.lane else ()),'leases':db.rows('SELECT id,attempt_id,heartbeat_at FROM slot')}
        if args.campaign:
            result['trials']=db.rows('SELECT id,experiment_id,replicate,status,order_index FROM trial WHERE campaign_id=? ORDER BY order_index',(args.campaign,))
            sequenced=db.rows('PRAGMA user_version')[0]['user_version']>=4
            result['execution_sequence_available']=sequenced
            query=('SELECT a.id,a.trial_id,a.number,a.status,a.exit_code,s.sequence,s.launch_sequence,s.dispatched_at,s.start_observed_at FROM attempt a LEFT JOIN execution_sequence s ON s.attempt_id=a.id JOIN trial t ON t.id=a.trial_id WHERE t.campaign_id=? ORDER BY s.sequence,a.started_at' if sequenced else 'SELECT a.id,a.trial_id,a.number,a.status,a.exit_code FROM attempt a JOIN trial t ON t.id=a.trial_id WHERE t.campaign_id=? ORDER BY t.order_index,a.number')
            result['attempts']=db.rows(query,(args.campaign,))
        return result
    if args.action=='run': return Executor(db).run(args.campaign,args.lane,args.max_cells)
    if args.action=='cancel':
        with db.transaction() as c:
            if not c.execute('SELECT 1 FROM slot WHERE attempt_id=?',(args.attempt,)).fetchone(): raise Refusal('attempt is not active')
            c.execute('UPDATE attempt SET cancel_requested=1 WHERE id=?',(args.attempt,))
        return {'attempt':args.attempt,'cancel_requested':True,'lease_retained_until_join':True}
    if args.action=='recover': return Executor(db).recover(args.attempt)
    if args.action=='cleanup':
        with file_lock(db.private/'controller.lock'), file_lock(db.private/'leases'/(args.attempt+'.lock')):
            with db.transaction() as c: db.quiescent(c)
            runner=Executor(db)
            try:
                resource=runner.resource(args.attempt)
                if not resource['drained_at']: raise Refusal('unverified drain; use recover')
                obj=runner.docker().inspect(resource['container_name'])
                if obj:
                    runner.docker().verify(obj,resource)
                    if obj['State']['Running']: raise Refusal('live process cleanup refused')
                    runner.retain(args.attempt,obj)
                runner.remove_owned(args.attempt)
                return {'attempt':args.attempt,'cleanup':'removed'}
            except BaseException as e:
                runner.cleanup_receipt(args.attempt,'requested-owned-volume','refused',type(e).__name__+': '+str(e)[:300]); raise
    if args.action=='retry':
        with file_lock(db.private/'controller.lock'), db.transaction() as c:
            db.quiescent(c)
            a=c.execute('SELECT * FROM attempt WHERE id=?',(args.attempt,)).fetchone()
            err=c.execute("SELECT * FROM error WHERE attempt_id=? AND role='primary' ORDER BY id LIMIT 1",(args.attempt,)).fetchone()
            if not a or a['number']!=1 or a['status'] not in ('interrupted','failed','blocked') or not err or not err['retryable']: raise Refusal('only a transient/interrupted first attempt can retry')
            if c.execute('SELECT 1 FROM attempt WHERE previous_id=?',(args.attempt,)).fetchone(): raise Refusal('retry already consumed')
            if a['driver_sha256']!=driver_hash(db.root): raise Refusal('changed driver is a new condition, not retry')
            c.execute("UPDATE trial SET status='pending' WHERE id=?",(a['trial_id'],))
        return {'trial':a['trial_id'],'retry_of':args.attempt,'scheduled':True}
    if args.action=='continue':
        from lib.recipes import prerequisite_artifacts
        count=0
        with file_lock(db.private/'controller.lock'), db.transaction() as c:
            db.quiescent(c)
            for trial in c.execute("SELECT t.id,e.* FROM trial t JOIN experiment e ON e.id=t.experiment_id WHERE t.campaign_id=? AND t.status='blocked' AND e.status='ready' AND NOT EXISTS(SELECT 1 FROM attempt a WHERE a.trial_id=t.id)",(args.campaign,)).fetchall():
                # Avoid duplicate 'id' column ambiguity by fetching the experiment explicitly.
                tid=trial[0]; eid=c.execute('SELECT experiment_id FROM trial WHERE id=?',(tid,)).fetchone()[0]
                exp=dict(c.execute('SELECT * FROM experiment WHERE id=?',(eid,)).fetchone())
                try:
                    from lib.recipes import validate_factors
                    validate_factors(exp)
                    prerequisite_artifacts(db,c,exp)
                except Refusal as e:
                    c.execute('INSERT INTO trial_incident(trial_id,stage,category,message,at) VALUES(?,?,?,?,?)',(tid,'continue','unsafe-prerequisite',str(e)[:400],utc()))
                    continue
                c.execute("UPDATE trial SET status='pending' WHERE id=?",(tid,)); count+=1
        return {'unblocked':count}
    if args.action=='reparse':
        from lib.parse import ingest
        with file_lock(db.private/'controller.lock'):
            with db.transaction() as c: db.quiescent(c)
            return ingest(db,args.attempt,strict=False)
    if args.action=='record-findings':
        from lib.publish import record_findings
        return record_findings(db,args.file)
    if args.action=='export':
        from lib.publish import export
        return export(db,args.output)
    if args.action=='restore':
        from lib.publish import restore
        return restore(db,args.name)
    if args.action=='backup':
        from lib.publish import backup
        return backup(db,args.name)

if __name__=='__main__':
    try:
        print(canonical(main()))
    except (Refusal,sqlite3.Error,OSError,ValueError) as e:
        print(json.dumps({'error':type(e).__name__,'message':str(e)}),file=sys.stderr)
        sys.exit(2)
