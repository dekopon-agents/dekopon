"""Closed recipe catalog. Workers cannot supply shell commands, env, mounts, or images."""
import json
from pathlib import Path
from .ledger import Refusal, canonical, digest, safe_path
from .docker import Missing

HISTORY_SHA='c892c88bd9a25110677b7652d535c846fd9c67c584a29336e63d2c49daff6ff2'
GUARD='''set -eu
umask 077
[ "$(cat /sys/fs/cgroup/memory.max)" = 1073741824 ]
[ "$(cat /sys/fs/cgroup/memory.swap.max)" = 0 ]
[ "$(cat /sys/fs/cgroup/pids.max)" = 64 ]
[ "$(cat /sys/fs/cgroup/cpu.max)" = "100000 100000" ]
free_kb=$(df -Pk /work/run | awk 'END {print $4}')
[ "$free_kb" -ge 8388608 ] || { echo 'resource-exhaustion: Docker disk below 8GiB'; exit 78; }
printf 'study-build-environment page_bytes='; getconf PAGESIZE
[ "$(uname -m)" = aarch64 ]
uname -m
printf 'study-build-disk-free-kb=%s\\n' "$free_kb"
mkdir target
export TMPDIR=/work/run/target
'''

# Closed compatibility policy: runtime image identity is NOT the producing compiler.
REQUIREMENTS={
    'release': {'collector':('builder-arm64-1.86',None)},
    'history': {'collector':('builder-arm64-1.86',None),'history':('builder','M')},
    'history-arm64-1.86': {'collector':('builder-arm64-1.86',None),'history':('builder-arm64-1.86','M')},
}
HISTORY_DEFAULTS={'keys':128,'turns':12,'window_bytes':65536,'user_bytes':2048,'answer_bytes':4096,'held_clones':4}

def source_hash(db,role):
    names=['collector.c'] if role=='collector' else ['history.rs']
    inputs={name:digest(safe_path(db.root,'recipes/'+name,True).read_bytes()) for name in names}
    if role=='history': inputs['history_source.rs']=HISTORY_SHA
    inputs['recipe_driver']=digest(safe_path(db.root,'lib/recipes.py',True).read_bytes())
    return digest(canonical(inputs).encode())

def register_versions(db,c):
    for role in ['collector','history']:
        c.execute('INSERT INTO binary_version VALUES(?,?) ON CONFLICT(role) DO UPDATE SET input_sha256=excluded.input_sha256',(role,source_hash(db,role)))
    for consumer,roles in REQUIREMENTS.items():
        for role,(producer,source) in roles.items():
            c.execute('INSERT OR IGNORE INTO binary_requirement VALUES(?,?,?,?)',(consumer,role,producer,source))

def validate_factors(exp):
    config=json.loads(exp['config_json']); rid=exp['recipe_id']
    if rid in ('history','history-smoke'):
        expected={**HISTORY_DEFAULTS,'instrumentation':'in-container-static-v1'}
    elif rid=='broker':
        if exp['provider_set_id'] not in ('P4','echo'): raise Refusal('unsupported provider set')
        expected={'provider_count':4 if exp['provider_set_id']=='P4' else 1,'compile_cache':'off','instrumentation':'in-container-static-v1','idle_seconds':60}
    elif rid in ('build-collector','build-history','smoke','watchdog','deadline-smoke'): expected={}
    else: raise Refusal('unsupported recipe; never substitute a toy workload')
    # Fixed screening recipes, not an arbitrary-factor runner. Type and unknown-key checks
    # happen BEFORE a slot is claimed; even Python True == 1 must not match.
    if canonical(config)!=canonical(expected): raise Refusal('unsupported factors for '+rid+'; new reviewed recipe required')
    return config

def prerequisite_artifacts(db,c,exp):
    result={}
    for row in c.execute('SELECT p.id,p.kind FROM experiment_prerequisite ep JOIN prerequisite p ON p.id=ep.prerequisite_id WHERE ep.experiment_id=?',(exp['id'],)):
        role=row['id']
        if row['kind']!='artifact': raise Missing('missing validated prerequisite: '+role)
        requirement=c.execute('SELECT * FROM binary_requirement WHERE consumer_build_id=? AND role=?',(exp['build_id'],role)).fetchone()
        if not requirement: raise Refusal('unsupported binary producer requirement: '+role)
        a=c.execute("""SELECT a.* FROM binary_product p JOIN artifact a ON a.id=p.artifact_id
            JOIN attempt producer ON producer.id=p.producer_attempt_id
            WHERE p.role=? AND p.build_id=? AND p.source_id IS ? AND p.input_sha256=? AND producer.status='succeeded'
            ORDER BY producer.ended_at DESC,a.id LIMIT 1""",(role,requirement['producer_build_id'],requirement['source_id'],source_hash(db,role))).fetchone()
        if not a: raise Missing('missing compatible '+role+' producer '+requirement['producer_build_id']+' at current source version')
        path=safe_path(db.root,a['relative_path'],True)
        if path.stat().st_size>8*1024*1024 or digest(path.read_bytes())!=a['sha256']: raise Refusal('prerequisite artifact hash mismatch: '+role)
        result[role]=dict(a)
    required=set(REQUIREMENTS.get(exp['build_id'],{})) if exp['recipe_id'] in ('history','history-smoke','broker','smoke','deadline-smoke') else set()
    if set(result)!=required: raise Refusal('missing declared binary prerequisite roles')
    return result

def prepare(db,exp,artifacts):
    rid=exp['recipe_id']; files={}; factors=validate_factors(exp)
    timeout=db.rows('SELECT timeout_s FROM recipe WHERE id=?',(rid,))[0]['timeout_s']
    def source(name):
        p=safe_path(db.root,'recipes/'+name,True)
        return p.read_bytes()
    def binary(name):
        a=artifacts[name]; data=safe_path(db.root,a['relative_path'],True).read_bytes()
        if digest(data)!=a['sha256']: raise Refusal('binary changed during input preparation')
        return data
    if rid=='build-collector':
        files['collector.c']=(source('collector.c'),0o400)
        cmd=['/bin/sh','-c',GUARD+'cc --version\ncc -std=c11 -O2 -Wall -Wextra -Werror -static collector.c -o target/collector\nprintf "study-build-complete\\n"\n']
    elif rid=='build-history':
        # Only this mapper-selected source seam is read. A changed source is a new condition.
        p=db.root.parents[1]/'crates/dekopon-agent/src/prompt/history.rs'
        data=p.read_bytes()
        if digest(data)!=HISTORY_SHA: raise Refusal('History source fence changed; new reviewed condition required')
        files['history_source.rs']=(data,0o400); files['history.rs']=(source('history.rs'),0o400)
        if exp['build_id'] not in ('builder','builder-arm64-1.86'): raise Refusal('unreviewed History compiler')
        version='1.86.0' if exp['build_id']=='builder-arm64-1.86' else '1.89.0'
        cmd=['/bin/sh','-c',GUARD+'rustc --version\n[ "$(rustc --version | cut -d \' \' -f 2)" = "'+version+'" ] || exit 78\nrustc --edition=2021 -O -C target-feature=+crt-static -C strip=symbols history.rs -o target/history\nprintf "study-build-complete\\n"\n']
    elif rid in ('smoke','deadline-smoke'):
        files['collector']=(binary('collector'),0o500); cmd=['/work/run/collector','--deadline',str(timeout),'--self-test' if rid=='smoke' else '--deadline-self-test']
    elif rid in ('history','history-smoke'):
        files['collector']=(binary('collector'),0o500); files['history']=(binary('history'),0o500)
        cmd=['/work/run/collector','--deadline',str(timeout),'--child','/work/run/history']+[str(factors[k]) for k in HISTORY_DEFAULTS]
    elif rid=='broker':
        files['collector']=(binary('collector'),0o500)
        config={'apiVersion':'dekopon.dev/brokerd/v1alpha1','socketPath':'/work/run/broker.sock','auditPath':'/work/run/audit.jsonl','checkpointPath':'/work/run/checkpoint.json','checkpointLockPath':'/work/run/checkpoint.lock','brokerPrincipal':'synthetic-broker','policyRevision':'synthetic-v1','policiesPath':'/work/run/policies.cedar',
            'providers':['/opt/dekopon/providers' if exp['provider_set_id']=='P4' else '/opt/dekopon/providers/echo-provider.wasm'],
            'identities':[{'uid':65532,'principal':'synthetic-user','actor':{'type':'human','principal':'synthetic-user'}}],
            'constraintSets':{'echo.echo':{'provider':'echo','effect':'read-only','risk':'Low','idempotency':'idempotent','constraints':{'timeoutMs':1000,'maxOutputBytes':4096}}}}
        policy='@id("synthetic-echo")\npermit(principal == Dekopon::Principal::"synthetic-user", action == Dekopon::Action::"echo.echo", resource == Dekopon::Provider::"echo") unless { context has via };\n'
        files['config.json']=(canonical(config).encode(),0o400); files['policies.cedar']=(policy.encode(),0o400)
        cmd=['/work/run/collector','--deadline',str(timeout),'--broker','/usr/local/bin/dekopon-brokerd','--config','/work/run/config.json']
    elif rid=='watchdog':
        cmd=['/bin/sh','-c',GUARD+'trap \'\' INT TERM; setsid /bin/sh -c \'trap "" INT TERM; echo study-noncooperative-descendant-started; while :; do sleep 1; done\' & wait\n']
    else: raise Refusal('unsupported recipe; never substitute a toy workload')
    if rid in ('build-collector','build-history','watchdog'):
        # GNU timeout is container PID1, not a Python-owned timer. --foreground keeps
        # timeout alive to reap a SIGKILLed child; its exit tears down the PID namespace,
        # including descendants that created a new session. No cooperative TERM grace.
        cmd=['/usr/bin/timeout','--foreground','--signal=KILL',str(timeout)]+cmd
    return cmd,files
