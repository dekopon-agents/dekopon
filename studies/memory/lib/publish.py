"""Deterministic relational, analysis-only export; deliberately cannot become an executor."""
import json
import re
import sqlite3
from .ledger import Refusal, canonical, digest, safe_path, file_lock

# Unstructured runtime strings are never published. Source/matrix/design fields are authored
# public inputs. Raw payloads, commands, machine/container identities remain private.
REDACT={
    'attempt':{'command_json':canonical({'redacted':True,'recipe_source':'recipes/; condition_sha256 retained'})},
    'trial_incident':{'message':'Private pre-claim cause retained locally; category published.'},
    'error':{'message':'Private failure detail retained locally; category and evidence hashes published.'},
    'recovery':{'reason':'Private ownership-verification detail retained locally.'},
    'cleanup':{'reason':'Private cleanup detail retained locally; action and evidence hashes published.'},
    'environment':{'load_json':'{}'},
}

def export(db,relative='exports/snapshot.sql'):
    output=safe_path(db.root,relative)
    if not relative.startswith('exports/') or not relative.endswith('.sql'): raise Refusal('exports/*.sql only')
    with file_lock(db.private/'controller.lock'):
        source=db.connect(); dest=None
        try:
            source.execute('BEGIN')
            db.quiescent(source)
            dest=sqlite3.connect(':memory:')
            source.backup(dest)
            dest.execute('PRAGMA foreign_keys=ON')
            triggers=dest.execute("SELECT name,sql FROM sqlite_master WHERE type='trigger' ORDER BY name").fetchall()
            for name,sql in triggers: dest.execute('DROP TRIGGER '+name)
            # Private executor ownership is excluded. Loading this SQL is analysis-only.
            dest.execute('DELETE FROM executor_identity')
            for table,fields in REDACT.items():
                for field,value in fields.items(): dest.execute(f'UPDATE {table} SET {field}=?',(value,))
            for table,field in [('environment','daemon_id'),('resource','daemon_id'),('resource','container_id'),('resource','container_name'),('resource','volume_name'),('resource','token')]:
                values=dest.execute(f'SELECT DISTINCT {field} FROM {table} WHERE {field} IS NOT NULL').fetchall()
                for (v,) in values: dest.execute(f'UPDATE {table} SET {field}=? WHERE {field}=?',('anonymous-'+digest(v.encode())[:24],v))
            # Cleanup targets can name private source paths in future versions: publish only hash.
            for id,target in dest.execute('SELECT id,target FROM cleanup').fetchall():
                dest.execute('UPDATE cleanup SET target=? WHERE id=?',('owned-target-'+digest(target.encode())[:24],id))
            for name,sql in triggers: dest.execute(sql)
            dest.commit()
            if dest.execute('PRAGMA foreign_key_check').fetchall(): raise Refusal('export foreign-key violation')
            # iterdump sorts tables/rowid consistently for an unchanged snapshot; repeat export is byte-identical.
            data=('-- Sanitized analysis-only relational snapshot. No executor identity or raw contents.\n'+'\n'.join(dest.iterdump())+'\nPRAGMA user_version = '+str(dest.execute('PRAGMA user_version').fetchone()[0])+';\n').encode()
            for sentinel in [b'/Users/',b'/home/',str(db.root).encode()]:
                if sentinel in data: raise Refusal('private path/host sentinel in export')
            # Match a local-DNS suffix, not SQL column names such as e.lane.
            if re.search(rb'\b[\w.-]+\.lan\b',data): raise Refusal('private path/host sentinel in export')
            output.parent.mkdir(exist_ok=True)
            tmp=output.with_suffix('.sql.new')
            if tmp.is_symlink(): raise Refusal('unsafe export temporary')
            with tmp.open('wb') as f: f.write(data)
            tmp.replace(output)
            return {'path':relative,'sha256':digest(data),'bytes':len(data),'analysis_only':True}
        finally:
            if dest is not None: dest.close()
            source.close()


def backup(db,name):
    if not name or any(ch not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for ch in name): raise Refusal('portable backup name required')
    with file_lock(db.private/'controller.lock'):
        c=db.connect()
        try:
            db.quiescent(c)
            folder=safe_path(db.root,'private/backups/'+name)
            folder.mkdir(mode=0o700,parents=True,exist_ok=False)
            destination=sqlite3.connect(folder/'ledger.sqlite3')
            c.backup(destination); destination.close()
            # Evidence remains at immutable relative paths; manifest makes an incomplete copy visible.
            rows=[dict(r) for r in c.execute('SELECT relative_path,sha256,bytes FROM artifact ORDER BY relative_path')]
            (folder/'manifest.json').write_text(canonical(rows)+'\n')
            return {'path':str(folder.relative_to(db.root)),'raw_files_to_copy':len(rows),'note':'Copy private/raw plus this database/manifest to independent storage; this database alone is not a complete backup.'}
        finally: c.close()


def restore(db,name):
    """Restore only an offline backup of THIS canonical inode/UUID. Never fork an executor."""
    import uuid
    if not name or any(ch not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for ch in name): raise Refusal('portable backup name required')
    folder=safe_path(db.root,'private/backups/'+name,True)
    path=safe_path(db.root,str((folder/'ledger.sqlite3').relative_to(db.root)),True)
    with file_lock(db.private/'controller.lock'):
        current=db.connect()
        source=sqlite3.connect('file:'+str(path)+'?mode=ro',uri=True)
        try:
            db.quiescent(current)
            original=tuple(current.execute('SELECT uuid,db_device,db_inode FROM executor_identity').fetchone())
            identity=source.execute('SELECT uuid,db_device,db_inode FROM executor_identity').fetchone()
            if not identity or tuple(identity)!=original: raise Refusal('backup is not this canonical executor; cross-filesystem relocation requires owner review')
            if source.execute('SELECT 1 FROM slot WHERE attempt_id IS NOT NULL').fetchone(): raise Refusal('backup contains unresolved leases')
            if source.execute('PRAGMA integrity_check').fetchone()[0]!='ok' or source.execute('PRAGMA foreign_key_check').fetchall(): raise Refusal('invalid backup')
            source.row_factory=sqlite3.Row
            rows=[dict(r) for r in source.execute('SELECT relative_path,sha256,bytes FROM artifact ORDER BY relative_path')]
            manifest=json.loads((folder/'manifest.json').read_text())
            if rows!=manifest: raise Refusal('backup manifest mismatch')
            for row in rows:
                data=safe_path(db.root,row['relative_path'],True).read_bytes()
                if len(data)!=row['bytes'] or digest(data)!=row['sha256']: raise Refusal('backup raw evidence missing or changed')
            # Preserve the current ledger before rollback, including every subsequent failure/claim.
            preserved=safe_path(db.root,'private/backups/before-restore-'+uuid.uuid4().hex)
            preserved.mkdir(mode=0o700)
            dst=sqlite3.connect(preserved/'ledger.sqlite3'); current.backup(dst); dst.close()
            previous=[dict(r) for r in current.execute('SELECT relative_path,sha256,bytes FROM artifact ORDER BY relative_path')]
            (preserved/'manifest.json').write_text(canonical(previous)+'\n')
            source.backup(current)
        finally:
            source.close();current.close()
        with db.transaction() as c:
            db.artifact(c,None,'restore-receipt','private/controller/'+uuid.uuid4().hex+'.json',canonical({'restored':name,'preserved':str(preserved.relative_to(db.root))}).encode())
        return {'restored':name,'preserved':str(preserved.relative_to(db.root)),'next':'check; migrate if pending; controller-sync; never drop retained raw files'}


def record_findings(db,relative):
    if relative!='findings.json' and not relative.startswith('findings/'):
        raise Refusal('findings.json or findings/*.json only')
    path=safe_path(db.root,relative,True)
    if path.stat().st_size>65536: raise Refusal('finding input exceeds 64KiB')
    findings=json.loads(path.read_text())
    if not isinstance(findings,list) or len(findings)>16: raise Refusal('at most sixteen findings')
    with file_lock(db.private/'controller.lock'),db.transaction() as c:
        db.quiescent(c)
        for f in findings:
            if set(f)!={'id','status','claim','limitation','evidence'} or not all(isinstance(f[k],str) and 0<len(f[k])<=4096 for k in ['id','status','claim','limitation']): raise Refusal('invalid finding fields')
            if len(f['id'])>64 or any(ch not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for ch in f['id']): raise Refusal('invalid finding ID')
            values=tuple(f[k] for k in ['id','status','claim','limitation'])
            old=c.execute('SELECT * FROM finding WHERE id=?',(f['id'],)).fetchone()
            if old and tuple(old)!=values: raise Refusal('finding corrections require a new ID')
            if not old:c.execute('INSERT INTO finding VALUES(?,?,?,?)',values)
            if not isinstance(f['evidence'],list) or not 1<=len(f['evidence'])<=32: raise Refusal('one to 32 evidence links required')
            for e in f['evidence']:
                if not isinstance(e,dict) or len(e)!=1 or next(iter(e)) not in ['reference_id','artifact_id','parse_id']: raise Refusal('one typed evidence reference required')
                field,value=next(iter(e.items()))
                c.execute('INSERT OR IGNORE INTO finding_evidence(finding_id,'+field+') VALUES(?,?)',(f['id'],value))
        return {'recorded':[f['id'] for f in findings]}
