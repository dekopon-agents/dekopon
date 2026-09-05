#!/usr/bin/env python3
"""OPT-IN Linux containment gate. Not unittest discovery; uses ONLY canonical leased cells.

Run after reviewed migration/controller sync and versioned collector build. This creates
four infrastructure trials (no benchmarks): kill/stop Python AFTER a non-cooperative
setsid descendant is observed, then demand independent PID1 deadline termination.
"""
import argparse
import multiprocessing as mp
import os
from pathlib import Path
import signal
import sys
import time
import uuid
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from lib.ledger import Ledger, ROOT, Refusal, canonical, driver_hash, file_lock
from lib.docker import Docker
from lib.executor import Executor


def owner(campaign,queue):
    db=Ledger()
    with file_lock(db.private/'controller.lock',exclusive=False):
        ex=Executor(db); claim=ex.claim(campaign,'infra')
        if claim is None: queue.put(None); return
        queue.put(claim['id'])
        ex.execute(claim)


def check_case(db,cell,mode):
    campaign='safety-'+uuid.uuid4().hex[:16]
    db.campaign(campaign,7,[cell],1)
    ctx=mp.get_context('spawn');queue=ctx.Queue();process=ctx.Process(target=owner,args=(campaign,queue))
    process.start();id=None;ex=Executor(db);engine=ex.docker()
    try:
        id=queue.get(timeout=20)
        if not id: raise Refusal('safety prerequisite blocked; no launch')
        end=time.monotonic()+30
        while time.monotonic()<end:
            resource=ex.resource(id)
            if resource['started_at']:
                obj=ex.snapshot(id)
                if obj and obj['State']['Running'] and b'study-noncooperative-descendant-started' in engine.logs(obj['Id']): break
            if not process.is_alive(): raise Refusal('owner exited before owner-loss injection; not a passing test')
            time.sleep(.05)
        else: raise Refusal('descendant launch not observed in time')
        # SIGSTOP removes the Python timer just as surely as owner death. Do not resume
        # it until independent containment has been observed; no parent-issued Docker kill.
        os.kill(process.pid,signal.SIGKILL if mode=='kill' else signal.SIGSTOP)
        expected=124 if cell=='INFRA-DEADLINE' else 137
        timeout=db.rows('SELECT r.timeout_s FROM recipe r JOIN experiment e ON e.recipe_id=r.id WHERE e.id=?',(cell,))[0]['timeout_s']
        end=time.monotonic()+timeout+15
        while time.monotonic()<end:
            obj=ex.snapshot(id)
            if obj and not obj['State']['Running'] and obj['State']['Status']=='exited': break
            time.sleep(.1)
        else: raise Refusal('container-owned deadline did not terminate; lease retained until recovery')
        if obj['State']['ExitCode']!=expected or obj['State'].get('OOMKilled') or obj['State'].get('Pid')!=0:
            raise Refusal('deadline/PID-namespace exit not verified')
        if not db.rows('SELECT 1 FROM slot WHERE attempt_id=?',(id,)):
            raise Refusal('lease released without owner recovery')
        ex.evidence(id,'safety-check','owner-loss.json',canonical({'mode':mode,'descendant_observed':True,'owner_timer_unavailable':True,'container_deadline_exit':expected,'pid_namespace_exited':True}).encode())
        if process.is_alive(): process.kill()
        process.join(10)
        result=ex.recover(id)
        if result['lease_held'] or ex.primary(id)[2:]!=('resource-exhaustion','OOM/SIGKILL or container-owned deadline (exit 124/137)',0):
            raise Refusal('termination/recovery classification gate failed')
        return {'cell':cell,'mode':mode,'attempt':id,'container_deadline_exit':expected,'lease_held':False}
    finally:
        if process.is_alive(): process.kill()
        process.join(10);queue.close()
        if id and db.rows('SELECT 1 FROM slot WHERE attempt_id=?',(id,)):
            # This is cleanup, never success evidence for the independent deadline test.
            result=ex.recover(id)
            if result['lease_held']: print(canonical(result),file=sys.stderr)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-synthetic-faults',action='store_true')
    args=parser.parse_args()
    if not args.run_synthetic_faults: parser.error('explicit --run-synthetic-faults required; this writes infrastructure trials')
    db=Ledger()
    if db.root!=ROOT: raise Refusal('canonical database only')
    with file_lock(db.private/'controller.lock'),db.transaction() as c:
        db.quiescent(c)
        if c.execute('PRAGMA user_version').fetchone()[0]!=db.migrations()[-1][0] or c.execute('SELECT driver_sha256 FROM executor_identity').fetchone()[0]!=driver_hash():
            raise Refusal('reviewed migration/controller-sync required')
    results=[check_case(db,cell,mode) for cell in ('INFRA-WATCHDOG','INFRA-DEADLINE') for mode in ('kill','stop')]
    print(canonical({'synthetic_linux_safety':results,'benchmarks_run':0}))


if __name__=='__main__': main()
