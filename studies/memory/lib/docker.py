"""Bounded local Docker Engine API. Never mounts host paths or inherits workload env."""
import http.client
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
from urllib.parse import quote, urlencode
from .ledger import Refusal, canonical

class Missing(Refusal): pass
class EngineError(Refusal): pass
class TransportError(EngineError): pass

class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost',timeout=10)
        self.path=path
    def connect(self):
        self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)

class Docker:
    def __init__(self):
        env={k:os.environ[k] for k in ['PATH','HOME'] if k in os.environ}
        try:
            p=subprocess.run(['docker','context','inspect','--format','{{.Endpoints.docker.Host}}'],env=env,capture_output=True,timeout=10,check=True)
        except (OSError,subprocess.SubprocessError) as e:
            raise EngineError('local Docker context unavailable') from e
        host=p.stdout.decode().strip()
        if not host.startswith('unix:///') or '\n' in host:
            raise Refusal('only a local Unix Docker daemon is supported; no remote/TCP fallback')
        self.socket=host[7:]
        info=self.call('GET','/info')
        self.id=info['ID']
        self.info={k:info.get(k) for k in ['ID','OSType','Architecture','Driver','MemTotal','NCPU','CgroupVersion']}
        if info.get('OSType')!='linux' or info.get('CgroupVersion')!='2':
            raise Refusal('Linux cgroup v2 required')

    def check_daemon(self, expected=None):
        # Query the endpoint, never treat the constructor's cached ID as an observation.
        info=self._call('GET','/info')
        if info.get('ID')!=(expected or self.id) or info.get('OSType')!='linux' or info.get('CgroupVersion')!='2':
            raise Refusal('daemon identity changed; original tree not observed')
        return info['ID']

    def call(self, method, path, obj=None, data=None, maximum=16*1024*1024):
        guarded=hasattr(self,'id')
        if guarded: self.check_daemon()
        try: return self._call(method,path,obj,data,maximum)
        finally:
            # Includes 404: absence on a replacement daemon proves nothing.
            if guarded: self.check_daemon()

    def _call(self, method, path, obj=None, data=None, maximum=16*1024*1024):
        headers={}
        if obj is not None:
            data=canonical(obj).encode(); headers['Content-Type']='application/json'
        elif data is not None: headers['Content-Type']='application/x-tar'
        conn=UnixConnection(self.socket)
        try:
            conn.request(method,'/v1.45'+path,body=data,headers=headers)
            response=conn.getresponse()
            body=response.read(maximum+1)
            if len(body)>maximum: raise EngineError('bounded Docker response exceeded')
            if response.status==404: raise Missing('Docker object absent')
            if response.status>=300:
                error=TransportError if response.status>=500 else EngineError
                raise error('Docker API refused: HTTP '+str(response.status)+' '+body[:512].decode(errors='replace'))
            if 'application/json' in (response.getheader('Content-Type') or ''):
                return json.loads(body) if body else None
            return body
        except (OSError,http.client.HTTPException,ValueError) as e:
            raise TransportError('Docker transport/response failure') from e
        finally: conn.close()

    def image(self, image):
        if '@sha256:' not in image: raise Refusal('image must be digest pinned')
        obj=self.call('GET','/images/'+quote(image,safe='')+'/json')
        return {k:obj.get(k) for k in ['Id','RepoDigests','Architecture','Os']}

    def inspect(self, name):
        try: return self.call('GET','/containers/'+name+'/json')
        except Missing: return None

    def volume(self,name):
        try: return self.call('GET','/volumes/'+name)
        except Missing: return None

    def create(self,name,volume,image,command,labels,recipe):
        if self.inspect(name) is not None or self.volume(volume) is not None:
            raise Refusal('owned resource name already exists; recover rather than reuse')
        self.call('POST','/volumes/create',obj={'Name':volume,'Labels':labels})
        config={'Image':image,'Cmd':command,'Entrypoint':[], 'User':'65532:65532',
            'WorkingDir':'/work/run','Env':['PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin','HOME=/work/run','LANG=C','LC_ALL=C','TZ=UTC']+(['RUSTUP_HOME=/usr/local/rustup','CARGO_HOME=/usr/local/cargo'] if image.startswith('rust@') else []),
            'Labels':labels,'NetworkDisabled':True,'AttachStdout':False,'AttachStderr':False,
            'HostConfig':{'Memory':recipe['memory_bytes'],'MemorySwap':recipe['memory_bytes'],
                'NanoCpus':int(recipe['cpus']*1e9),'PidsLimit':recipe['pids'],'ReadonlyRootfs':True,
                'NetworkMode':'none','CapDrop':['ALL'],'SecurityOpt':['no-new-privileges:true'],
                'RestartPolicy':{'Name':'no'},'Privileged':False,'AutoRemove':False,'Init':False,
                'Ulimits':[{'Name':'nofile','Soft':256,'Hard':256},{'Name':'fsize','Soft':16777216,'Hard':16777216},{'Name':'core','Soft':0,'Hard':0}],
                'Mounts':[{'Type':'volume','Source':volume,'Target':'/work','VolumeOptions':{'NoCopy':True}}],
                'LogConfig':{'Type':'json-file','Config':{'max-size':'4m','max-file':'1'}}}}
        return self.call('POST','/containers/create?'+urlencode({'name':name,'platform':'linux/arm64'}),obj=config)['Id']

    def verify(self,obj,resource,recipe=None):
        labels=obj.get('Config',{}).get('Labels') or {}
        if obj['Name']!='/'+resource['container_name'] or labels.get('memory-study.token')!=resource['token'] or labels.get('memory-study.attempt')!=resource['attempt_id']:
            raise Refusal('container ownership labels/name mismatch')
        if resource.get('container_id') and obj['Id']!=resource['container_id']: raise Refusal('container ID mismatch')
        if resource.get('created_at') and obj['Created']!=resource['created_at']: raise Refusal('container creation identity mismatch')
        started=obj['State']['StartedAt']
        if resource.get('started_at') and started!=resource['started_at']: raise Refusal('container start identity changed; possible external restart')
        self.check_daemon(resource['daemon_id'])
        if recipe:
            h=obj['HostConfig']
            expected={'Memory':recipe['memory_bytes'],'MemorySwap':recipe['memory_bytes'],'NanoCpus':int(recipe['cpus']*1e9),'PidsLimit':recipe['pids'],'ReadonlyRootfs':True,'NetworkMode':'none','Privileged':False,'AutoRemove':False}
            if any(h.get(k)!=v for k,v in expected.items()) or h.get('RestartPolicy',{}).get('Name')!='no': raise Refusal('Docker limits/isolation mismatch')
            if h.get('Init') or h.get('PidMode') or h.get('Binds') or h.get('Devices') or len(obj['Mounts'])!=1: raise Refusal('unexpected host access or mounts')
            mount=obj['Mounts'][0]
            if mount['Type']!='volume' or mount['Name']!=resource['volume_name'] or mount['Destination']!='/work': raise Refusal('unexpected mount')
            if obj['Config']['User']!='65532:65532' or h.get('CapDrop')!=['ALL'] or not any('no-new-privileges' in v for v in h.get('SecurityOpt',[])): raise Refusal('privilege isolation mismatch')
        return obj

    def upload(self,id,files):
        out=io.BytesIO()
        with tarfile.open(fileobj=out,mode='w') as tar:
            directory=tarfile.TarInfo('run'); directory.type=tarfile.DIRTYPE; directory.mode=0o700; directory.uid=directory.gid=65532; tar.addfile(directory)
            for name,(data,mode) in sorted(files.items()):
                if Path(name).is_absolute() or '..' in Path(name).parts: raise Refusal('unsafe upload member')
                t=tarfile.TarInfo('run/'+name); t.size=len(data); t.mode=mode; t.uid=t.gid=65532
                tar.addfile(t,io.BytesIO(data))
        self.call('PUT','/containers/'+id+'/archive?path=/work&copyUIDGID=true',data=out.getvalue())

    def start(self,id): self.call('POST','/containers/'+id+'/start')
    def stop(self,id): self.call('POST','/containers/'+id+'/kill?signal=SIGKILL')
    def remove(self,id): self.call('DELETE','/containers/'+id+'?force=false&v=false')
    def remove_volume(self,name): self.call('DELETE','/volumes/'+name)
    def archive(self,id,path): return self.call('GET','/containers/'+id+'/archive?'+urlencode({'path':path}),maximum=64*1024*1024 if path.startswith('/usr/') or path.startswith('/opt/') else 16*1024*1024)
    def logs(self,id):
        data=self.call('GET','/containers/'+id+'/logs?stdout=true&stderr=true&timestamps=false')
        result=bytearray()
        # Non-TTY Docker logs carry 8-byte multiplex framing; never parse headers as measurements.
        while data:
            if len(data)<8 or data[0] not in (1,2): raise EngineError('invalid log framing')
            size=int.from_bytes(data[4:8],'big')
            if size>len(data)-8: raise EngineError('truncated log framing')
            result+=data[8:8+size]; data=data[8+size:]
        return bytes(result)

def archive_file(data,basename):
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        members=tar.getmembers()
        if len(members)!=1 or not members[0].isfile() or Path(members[0].name).name!=basename or members[0].size>8*1024*1024:
            raise Refusal('unexpected retained binary archive')
        f=tar.extractfile(members[0])
        if f is None: raise Refusal('missing regular artifact')
        return f.read(8*1024*1024+1)
