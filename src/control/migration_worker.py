"""Fixed scoped migration effects. Chunk I/O is served from attested Rust FDs.

No caller paths, command strings, native API tokens or arbitrary disks are
accepted. A source is never unlocked after a committed ownership decision.
"""
import hashlib
import ctypes
import signal
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys

MIB=1024**2
VOLUME_KEYS=('type','guid','createtxg','volsize','refreservation','origin','org.pve-dev:owner','org.pve-dev:image','org.pve-dev:migration','readonly')
ALLOWED={'name','description','cores','memory','balloon','cpu','kvm','scsihw','cpuunits','net0','ipconfig0','citype','ciupgrade','nameserver','searchdomain','sshkeys','serial0','vga','smbios1','vmgenid','boot','scsi0','ide2','ostype','meta','protection','onboot','digest','lock','cicustom'}

def require(value,reason):
    if not value: raise RuntimeError(reason)
child=None
def stop_child(signum,frame):
    global child
    if child is not None and child.poll() is None:
        os.killpg(child.pid,signal.SIGTERM)
        try:child.wait(timeout=10)
        except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
    raise RuntimeError('migration adapter interrupted; inspect durable intent before any recovery')
def run(*args,allowed=(0,),timeout=3600):
    global child
    child=subprocess.Popen(args,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True,env={'PATH':'/usr/sbin:/usr/bin:/sbin:/bin','LC_ALL':'C'})
    try:stdout,stderr=child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:stop_child(signal.SIGALRM,None)
    status=child.returncode;child=None
    require(status in allowed,args[0]+' failed (exit '+str(status)+'): '+stderr.strip()[-1500:]+'; preserve migration journal')
    return stdout.strip()
def api(method,api_path,**values):
    args=['pvesh',method,api_path,'--output-format','json']
    for key,value in values.items():args+=['--'+key.replace('_','-'),str(value)]
    result=run(*args)
    return None if method=='set' else json.loads(result) if result else None

def sha(path):
    h=hashlib.sha256()
    with open(path,'rb',buffering=0) as f:
        for chunk in iter(lambda:f.read(4*MIB),b''):h.update(chunk)
    return h.hexdigest()
def file_proof(path):
    info=path.stat()
    require(path.resolve()==path and path.is_file() and info.st_uid==0 and info.st_nlink==1,'unsafe owned metadata file')
    return {'path':str(path),'bytes':info.st_size,'guid':str(info.st_dev),'createtxg':str(info.st_ino),'sha256':sha(path),'regular':True}
def metadata_base(p):return Path(p['state_dir'])/'migration-data'/p['migration_id']
def metadata_storage(p):return (p['binding'].get('physical_trial') or {}).get('scope',{}).get('storage_prefix','ctl-')+'meta-'+p['migration_id'][:8]
def allocate_metadata(p,files):
    root=Path(p['state_dir'])/'migration-data'
    root.mkdir(mode=0o700,exist_ok=True)
    require(root.resolve()==root and root.stat().st_uid==0 and not root.stat().st_mode&0o077,'unsafe migration metadata root')
    base=metadata_base(p);base.mkdir(mode=0o700,exist_ok=False)
    snippets=base/'snippets';snippets.mkdir(mode=0o700)
    identity=base/'identity.json'
    with identity.open('x') as f:json.dump({'migration_id':p['migration_id'],'plan_sha256':p['plan_sha256']},f);f.flush();os.fsync(f.fileno())
    proofs={}
    for slot,contents in files.items():
        require(slot in ('meta','user','network'),'unsupported metadata slot')
        size=contents if isinstance(contents,int) else len(contents)
        require(0<size<=64*1024,'metadata exceeds bound')
        path=snippets/(slot+'.yaml');fd=os.open(path,os.O_RDWR|os.O_CREAT|os.O_EXCL,0o600)
        try:
            if isinstance(contents,int):os.ftruncate(fd,size)
            else:
                with os.fdopen(os.dup(fd),'wb') as f:f.write(contents);f.flush()
            os.fsync(fd)
        finally:os.close(fd)
        proofs[slot]=file_proof(path)
    for directory in (snippets,base,root):
        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY)
        try:os.fsync(fd)
        finally:os.close(fd)
    return {'identity':file_proof(identity),'files':proofs}
def props(name):
    return dict(row.split('\t',1) for row in run('zfs','get','-Hp','-o','property,value',','.join(VOLUME_KEYS),name).splitlines())
def names(name):return run('zfs','list','-H','-r','-t','filesystem,volume,snapshot,bookmark','-o','name',name).splitlines()
def normalized(config):
    result={k:(('1' if v else '0') if isinstance(v,bool) else str(v).strip() if k=='description' else str(v).removeprefix('current=') if k=='memory' else str(v)) for k,v in config.items() if k not in ('digest','lock')}
    if 'ide2' in result:result['ide2']=','.join(v for v in result['ide2'].split(',') if not v.startswith('size='))
    return result

def volume_device(name,size):
    run('udevadm','settle')
    link=Path('/dev/zvol')/name;device=link.resolve(strict=True)
    info=device.stat()
    require(re.fullmatch(r'/dev/zd[0-9]+',str(device)) and stat.S_ISBLK(info.st_mode),'unsupported non-zvol device')
    require(int(run('blockdev','--getsize64',str(device)))==size,'block-device size changed')
    data=json.loads(run('lsblk','-J','-p','-o','NAME,MOUNTPOINTS',str(device)))
    def unused(item):
        require(not any(item.get('mountpoints') or []),'migration disk or partition mounted')
        require(not list((Path('/sys/class/block')/Path(item['name']).name/'holders').iterdir()),'migration disk has a holder')
        for child in item.get('children',[]):unused(child)
    for item in data['blockdevices']:unused(item)
    return {'path':str(device),'rdev':info.st_rdev,'bytes':size}

def bind(p):
    b=p['binding'];scope=(b.get('physical_trial') or {}).get('scope')
    env=b['environment'];require(re.fullmatch('[0-9a-f]{32}',env),'invalid migration environment')
    node=scope['node'] if scope else 'pve-lab';pool=scope['pool'] if scope else 'labpool'
    require(socket.gethostname()==node,'migration host identity mismatch')
    require(run('zpool','get','-Hp','-o','value','guid',pool)==b['pool_guid'],'migration pool identity mismatch')
    if scope:
        require(hashlib.sha256(Path('/etc/machine-id').read_bytes()).hexdigest()==scope['machine_id_sha256'],'physical machine identity changed')
        root=scope['root_dataset']
        require(root==pool+'/pve-dev-trial-'+env,'invalid physical migration namespace')
        identity=dict(row.split('\t',1) for row in run('zfs','get','-Hp','-o','property,value','guid,org.pve-dev:trial,quota,reservation',root).splitlines())
        require(identity=={'guid':scope['root_guid'],'org.pve-dev:trial':env,'quota':str(scope['capacity_mib']*MIB),'reservation':str(scope['capacity_mib']*MIB)},'physical root scope changed')
    else:
        require(Path('/etc/pve-dev-lab-id').read_text().strip()==env,'lab identity changed')
    vm_id=p['vm_id'];owner=p['owner'];require(isinstance(vm_id,int) and 9001<=vm_id<=9099 and re.fullmatch('[a-z][a-z0-9-]{0,47}',owner),'invalid migration VM/owner')
    if scope:require(vm_id in scope['vm_ids'] and owner in scope['owners'],'VM/owner escapes physical trial')
    control=scope['root_dataset']+'/control' if scope else 'labpool/control'
    p['node']=node;p['pool']=pool;p['vm_path']=f'/nodes/{node}/qemu/{vm_id}';p['dataset']=control+'/'+owner
    p['storage']=(scope['storage_prefix'] if scope else 'ctl-')+owner
    p['volume_names']={'root':p['dataset']+f'/vm-{vm_id}-disk-0','cloudinit':p['dataset']+f'/vm-{vm_id}-cloudinit'}
    state=Path(p['state_dir']);require(state.is_absolute() and state.resolve()==state and state.stat().st_uid==0 and not state.stat().st_mode&0o077,'unsafe helper state directory')
    return p

def snippet_sources(p,config):
    out={}
    for ref in str(config.get('cicustom','')).split(','):
        if not ref:continue
        slot,sep,volume=ref.partition('=');require(sep and slot in ('meta','user','network') and slot not in out,'unsupported custom cloud-init reference')
        storage,sep,relative=volume.partition(':');require(sep and re.fullmatch(r'snippets/[A-Za-z0-9._-]{1,128}',relative),'invalid custom cloud-init path')
        scope=(p['binding'].get('physical_trial') or {}).get('scope')
        prefix=scope['storage_prefix'] if scope else 'ctl-'
        require(storage.startswith(prefix),'custom snippet is outside migration-owned storage namespace')
        entry=api('get','/storage/'+storage)
        require(entry['type']=='dir' and 'snippets' in str(entry.get('content','')).split(','),'custom snippet storage is not a directory/snippet store')
        root=Path(entry['path']).resolve(strict=True)
        base=Path(p['state_dir']).parent
        require(root.is_relative_to(base),'custom snippet storage is outside private test state')
        path=(root/relative).resolve(strict=True);info=path.stat()
        require(path.is_relative_to(root) and path.is_file() and not (root/relative).is_symlink() and info.st_uid==0 and info.st_nlink==1 and 0<info.st_size<=64*1024,'unsafe custom snippet dependency')
        out[slot]={'path':str(path),'bytes':info.st_size,'guid':str(info.st_dev),'createtxg':str(info.st_ino),'sha256':sha(path)}
    return out

def snapshot(p,allow_locked=False):
    cfgs=[api('get',p['vm_path']+'/config',current=v) for v in (0,1)]
    require(normalized(cfgs[0])==normalized(cfgs[1]),'pending VM configuration refused')
    cfg=cfgs[1];require(set(cfg)<=ALLOWED,'unsupported VM hooks/devices/configuration')
    require(cfg.get('lock') in (None,'migrate') if allow_locked else not cfg.get('lock'),'VM is locked by another operation')
    status=api('get',p['vm_path']+'/status/current');require(status['status']=='stopped','migration VM must be stopped')
    require(not Path(f"/run/qemu-server/{p['vm_id']}.pid").exists(),'migration VM process is still present')
    require(normalized(cfg).get('protection','0')=='1','migration VM protection disabled')
    require(str(cfg.get('scsi0','')).split(',')[0]==p['storage']+f":vm-{p['vm_id']}-disk-0",'root disk mapping changed')
    require(normalized(cfg).get('ide2')==p['storage']+f":vm-{p['vm_id']}-cloudinit,media=cdrom",'cloud-init disk mapping changed')
    require(str(cfg.get('smbios1','')).startswith('uuid='),'source UUID missing')
    require(not any(v.get('sid')==f"vm:{p['vm_id']}" for v in api('get','/cluster/ha/resources')),'HA-managed VM migration refused')
    volumes={};devices={}
    for slot,name in p['volume_names'].items():
        require(names(name)==[name],'snapshots, bookmarks and children refused')
        v=props(name);size=int(v['volsize'])
        require(v['type']=='volume' and v['origin']=='-' and int(v['refreservation'])>=size,'migration requires independent thick volumes')
        owner_marker=f"pve-dev-control:{p['binding']['environment']}:{p['owner']}"
        # Host::Step::Volume explicitly stamps the root with vm_marker();
        # Proxmox-created cloud-init inherits owner_marker() from its dataset.
        expected_owner=owner_marker+':'+str(p['vm_id']) if slot=='root' else owner_marker
        require(v['org.pve-dev:owner']==expected_owner,'volume ownership changed for '+slot)
        require(size==(p['disk_mib']*MIB if slot=='root' else 4*MIB),'volume size differs from approved profile')
        volumes[slot]=v;devices[slot]=volume_device(name,size)
    for resource in api('get','/cluster/resources',type='vm'):
        require(resource['type']=='qemu','unsupported guest type while checking references')
        for current in (0,1):
            other=api('get',f"/nodes/{resource['node']}/qemu/{resource['vmid']}/config",current=current)
            for key,value in other.items():
                if any(name in str(value) for name in p['volume_names'].values()) or p['storage']+':' in str(value):
                    require(resource['node']==p['node'] and int(resource['vmid'])==p['vm_id'] and key in ('scsi0','ide2'),'foreign current/pending reference refused')
    snippets=snippet_sources(p,cfg)
    for slot,v in snippets.items():devices[slot]={**{k:v[k] for k in ('path','bytes','guid','createtxg')},'regular':True}
    config_path=Path(f"/etc/pve/qemu-server/{p['vm_id']}.conf").resolve(strict=True)
    result={'config':normalized(cfg),'raw_config':cfg,'volumes':volumes,'devices':devices,'snippets':snippets,'configuration_file':str(config_path),'configuration_file_sha256':sha(config_path)}
    if 'meta' not in snippets:
        # Proxmox derives generated instance identity from VM-local state. Pin
        # its exact source metadata as a custom dependency before changing VMID.
        meta=(run('qm','cloudinit','dump',str(p['vm_id']),'meta')+'\n').encode()
        require(0<len(meta)<=64*1024,'generated cloud-init metadata exceeds bound')
        result['generated_meta']=meta.decode()
    return result

def stable(p,record,locked=True,source=False):
    current=snapshot(p,allow_locked=locked)
    expected=dict(record['config_before'])
    if locked:expected['onboot']='0'
    require(current['config']==expected,'migration configuration drifted')
    if locked:require(current['raw_config'].get('lock')=='migrate','persistent migration fence disappeared')
    for slot,v in current['volumes'].items():
        before=record['volumes_before'][slot]
        require(all(v[k]==before[k] for k in ('type','guid','createtxg','volsize','refreservation','origin','org.pve-dev:owner','org.pve-dev:image')),'migration volume identity/backing changed')
        require(v['readonly']==('on' if source else 'off'),'migration volume readonly policy changed')
        if locked:require(v['org.pve-dev:migration']==p['migration_id'],'migration volume marker changed')
    return current

def fence(p,record):
    current=snapshot(p)
    require(current['config']==record['config_before'] and current['volumes']==record['volumes_before'],'source changed after preparation')
    api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],lock='migrate',onboot=0)
    for name in p['volume_names'].values():run('zfs','set','readonly=on','org.pve-dev:migration='+p['migration_id'],name)
    current=stable(p,record,True,True)
    if current.get('generated_meta'):
        require(current['generated_meta']==record['native'].get('generated_meta'),'source instance metadata changed after review')
        owned=allocate_metadata(p,{'meta':current['generated_meta'].encode()})
        current['owned_metadata']=owned
        current['devices'].update(owned['files']);current['snippets'].update(owned['files'])
    volumes={slot:{'bytes':device['bytes'],'sha256':sha(device['path']),'guid':current['volumes'][slot]['guid'],'createtxg':current['volumes'][slot]['createtxg']} for slot,device in current['devices'].items() if slot in p['volume_names']}
    snippets={slot:{k:v[k] for k in ('bytes','sha256','guid','createtxg')} for slot,v in current['snippets'].items()}
    manifest={'plan_sha256':p['plan_sha256'],'source_fence':{'migration_id':p['migration_id'],'lock':'migrate','onboot':0,'stopped':True,'volumes':current['volumes']},'config':record['config_before'],'volumes':volumes}
    if snippets:manifest['snippets']=snippets
    return {'manifest':manifest,'native':current}

def prepare_target(p,record,manifest):
    current=snapshot(p)
    require(current['config']==record['config_before'] and current['volumes']==record['volumes_before'],'target changed after creation')
    api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],lock='migrate',onboot=0)
    for name in p['volume_names'].values():run('zfs','set','readonly=off','org.pve-dev:migration='+p['migration_id'],name)
    current=stable(p,record,True,False)
    if manifest.get('snippets'):
        owned=allocate_metadata(p,{slot:item['bytes'] for slot,item in manifest['snippets'].items()})
        current['owned_metadata']=owned;current['devices'].update(owned['files'])
    return {'native':current}

def target_config(p,manifest,current):
    source=manifest['config'];require(set(source)<=ALLOWED and not ({'args','hookscript','hostpci0','usb0'}&set(source)),'unsupported source configuration')
    result=dict(source);result.pop('digest',None);result.pop('lock',None)
    result['name']=p['plan']['name'];result['description']=f"pve-dev-control:{p['binding']['environment']}:{p['owner']}:{p['vm_id']}"
    root=source['scsi0'].split(',');root[0]=p['storage']+f":vm-{p['vm_id']}-disk-0";result['scsi0']=','.join(root)
    result['ide2']=p['storage']+f":vm-{p['vm_id']}-cloudinit,media=cdrom"
    result['protection']='1';result['onboot']='0'
    if 'meta' in current:result['meta']=current['meta']
    else:result.pop('meta',None)
    if 'net0' in result:
        fields=dict(part.split('=',1) for part in result['net0'].split(','));require(set(fields)=={'virtio','bridge'},'unsupported source network mapping')
        require(re.fullmatch(r'(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}',fields['virtio']),'invalid network MAC')
        result['net0']='virtio='+fields['virtio']+',bridge='+p['plan']['target_bridge']
    snippets=manifest.get('snippets',{})
    if snippets:
        storage=metadata_storage(p);base=metadata_base(p)
        entries=api('get','/storage');require(not any(e['storage']==storage for e in entries),'snippet storage identity already occupied')
        api('create','/storage',storage=storage,type='dir',path=str(base),content='snippets',nodes=p['node'])
        result['cicustom']=','.join(slot+'='+storage+':snippets/'+slot+'.yaml' for slot in sorted(snippets))
    else:require('cicustom' not in result,'custom cloud-init manifest missing dependencies')
    return result

def seal(p,record,manifest):
    current=stable(p,record,True,False)
    native=p['native']
    for slot,item in {**manifest['volumes'],**manifest.get('snippets',{})}.items():
        device=native['devices'][slot]
        require(sha(device['path'])==item['sha256'],'target data hash differs from stopped source')
    expected=target_config(p,manifest,current['config'])
    arguments={key:value for key,value in expected.items() if current['config'].get(key)!=value}
    deleted=set(current['config'])-set(expected)-{'digest','lock','meta','protection'}
    if deleted:arguments['delete']=','.join(sorted(deleted))
    api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],skiplock=1,**arguments)
    if p['plan'].get('guest'):
        image=p['plan']['guest']['image_sha256'];require(re.fullmatch('[0-9a-f]{64}',image),'invalid approved image provenance')
        run('zfs','set','org.pve-dev:image='+image,p['volume_names']['root'])
    for name in p['volume_names'].values():run('zfs','set','readonly=on',name)
    updated=snapshot(p,allow_locked=True)
    if native.get('owned_metadata'):updated['owned_metadata']=native['owned_metadata']
    require(updated['config']==normalized(expected) and updated['raw_config']['lock']=='migrate','sealed target configuration mismatch')
    require(all(v['readonly']=='on' for v in updated['volumes'].values()),'sealed target is not readonly')
    for slot,item in manifest['volumes'].items():require(sha(updated['devices'][slot]['path'])==item['sha256'],'target changed during sealing')
    return {'config':updated['config'],'native':updated,'volumes':updated['volumes'],'source_manifest':p['manifest_sha256']}

def activate(p,record):
    current=snapshot(p,allow_locked=True)
    target=record['target_identities'];require(current['config']==target['config'],'target configuration changed before handoff activation')
    for slot,v in current['volumes'].items():
        old=target['volumes'][slot];require(all(v[k]==old[k] for k in ('guid','createtxg','volsize','refreservation','org.pve-dev:owner','org.pve-dev:image')),'target identity changed before activation')
    require(current['raw_config'].get('lock') in (None,'migrate'),'target has a foreign lock')
    for name in p['volume_names'].values():run('zfs','set','readonly=off',name)
    if current['raw_config'].get('lock')=='migrate':api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],delete='lock',skiplock=1)
    after=snapshot(p)
    require(after['config']==target['config'] and all(v['readonly']=='off' for v in after['volumes'].values()),'target activation state mismatch')
    if record['native'].get('owned_metadata'):after['owned_metadata']=record['native']['owned_metadata']
    return {'activated':True,'config':after['config'],'native':after}

def absence(p,record):
    require(not Path(f"/etc/pve/qemu-server/{p['vm_id']}.conf").exists() and not Path(f"/run/qemu-server/{p['vm_id']}.pid").exists(),'VM configuration/process still exists')
    require(not any(str(task.get('id',''))==str(p['vm_id']) for task in api('get',f"/nodes/{p['node']}/tasks",source='active')),'native VM task is still active')
    live=[row.split('\t') for row in run('zfs','list','-H','-p','-t','filesystem,volume,snapshot','-o','name,guid').splitlines()]
    old_guids={v['guid'] for v in record['volumes_before'].values()}
    require(all(name not in p['volume_names'].values() and guid not in old_guids for name,guid in live),'owned volume identity still exists, possibly renamed')
    return {'vm_absent':True,'volumes_absent':True,'native_tasks_absent':True}

def cleanup_metadata(p,record):
    # Never recurse: a storage entry and each individual file must match the
    # durable creation proof. Missing objects are safe lost-ack continuations.
    absence(p,record)
    owned=(record.get('native') or {}).get('owned_metadata')
    if not owned:return {'metadata_absent':True}
    base=metadata_base(p);identity=base/'identity.json'
    proof=owned['identity'];require(proof['path']==str(identity),'metadata proof escapes immutable migration namespace')
    if identity.exists():
        actual=file_proof(identity);require(all(actual[k]==proof[k] for k in ('path','guid','createtxg','bytes','sha256')),'metadata ownership marker changed')
        require(json.loads(identity.read_text())=={'migration_id':p['migration_id'],'plan_sha256':p['plan_sha256']},'metadata plan marker changed')
    else:require(not any(Path(v['path']).exists() for v in owned['files'].values()),'metadata files remain without ownership marker')
    storage=metadata_storage(p)
    entries=api('get','/storage')
    if any(e['storage']==storage for e in entries):
        require(record['role']=='destination','source metadata unexpectedly has a native storage registration')
        entry=api('get','/storage/'+storage)
        require(entry['type']=='dir' and entry['path']==str(base) and entry.get('content')=='snippets' and entry.get('nodes')==p['node'],'metadata storage registration changed')
        for resource in api('get','/cluster/resources',type='vm'):
            require(resource['type']=='qemu','unsupported guest type while checking snippet references')
            for current in (0,1):
                config=api('get',f"/nodes/{resource['node']}/qemu/{resource['vmid']}/config",current=current)
                require(not any(storage+':' in str(v) for v in config.values()),'metadata storage still referenced by a guest')
        api('delete','/storage/'+storage)
    for slot,item in owned['files'].items():
        path=base/'snippets'/(slot+'.yaml');require(slot in ('meta','user','network') and item['path']==str(path),'metadata file proof escapes migration namespace')
        if path.exists():
            actual=file_proof(path)
            # Receiving-stage zero contents are intentionally mutable; identity
            # and exact bounded size are invariant, while sealed data has hashes.
            require(all(actual[k]==item[k] for k in ('path','guid','createtxg','bytes')),'metadata inode changed')
            if record.get('manifest') and slot in record['manifest'].get('snippets',{}):
                require(actual['sha256']==record['manifest']['snippets'][slot]['sha256'] or record['phase'] not in ('complete','accepted','relinquished'),'sealed metadata contents changed')
            path.unlink()
    directory=base/'snippets'
    if directory.exists():directory.rmdir()
    if identity.exists():identity.unlink()
    if base.exists():base.rmdir()
    return {'metadata_absent':True}

def restored(p,record):
    current=snapshot(p)
    require(current['config']==record['config_before'],'original source configuration not fully restored')
    require(current['volumes']==record['volumes_before'],'original source volume properties not fully restored')
    return {'aborted':True,'source_restored':True}

def verify_sealed(p,record):
    current=snapshot(p,allow_locked=True);expected=record['target_identities']
    require(current['config']==expected['config'] and current['raw_config'].get('lock')=='migrate','sealed target fence/configuration changed')
    for slot,item in record['manifest']['volumes'].items():
        old=expected['volumes'][slot];actual=current['volumes'][slot]
        require(actual==old and actual['readonly']=='on' and sha(current['devices'][slot]['path'])==item['sha256'],'sealed target identity/data changed')
    for slot,item in record['manifest'].get('snippets',{}).items():require(sha(current['devices'][slot]['path'])==item['sha256'],'sealed metadata changed')
    if record['native'].get('owned_metadata'):current['owned_metadata']=record['native']['owned_metadata']
    return {'config':current['config'],'native':current,'volumes':current['volumes'],'source_manifest':p['manifest_sha256']}

def cleanup(p,record,abort=False):
    source=record['role']=='source'
    if abort and source:
        current=stable(p,record,True,True)
        for name in p['volume_names'].values():run('zfs','set','readonly=off',name);run('zfs','inherit','org.pve-dev:migration',name)
        values={'delete':'lock','skiplock':1,'digest':current['raw_config']['digest']}
        if 'onboot' in record['config_before']:values['onboot']=record['config_before']['onboot']
        else:values['delete']='lock,onboot'
        api('set',p['vm_path']+'/config',**values)
        require(snapshot(p)['config']==record['config_before'],'source abort did not restore original configuration')
        return {'aborted':True,'source_restored':True}
    current=snapshot(p,allow_locked=True)
    expected=record['config_before'] if source else (record.get('target_identities') or {}).get('config',dict(record['config_before'],onboot='0'))
    expected=dict(expected);expected['onboot']='0'
    require(current['config']==expected and current['raw_config'].get('lock')=='migrate','cleanup target changed or fence missing')
    for slot,value in current['volumes'].items():
        before=record['volumes_before'][slot];require(all(value[k]==before[k] for k in ('guid','createtxg','volsize','refreservation')),'cleanup volume identity changed')
        if source:require(value['readonly']=='on' and sha(current['devices'][slot]['path'])==record['manifest']['volumes'][slot]['sha256'],'retained source data changed')
    api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],skiplock=1,protection=0)
    run('qm','destroy',str(p['vm_id']),'--skiplock','1','--purge','0','--destroy-unreferenced-disks','0')
    require(not Path(f"/etc/pve/qemu-server/{p['vm_id']}.conf").exists(),'VM configuration remains after destruction')
    remaining=run('zfs','list','-H','-t','volume','-o','name').splitlines();require(not any(name in remaining for name in p['volume_names'].values()),'owned disks remain after destruction')
    result=absence(p,record);result.update(cleanup_metadata(p,record));result['cleaned']=True
    return result

def reopen(p,record):
    source=record['role']=='source'
    current=stable(p,record,True,source)
    original=record['native']
    for slot,item in record['manifest'].get('snippets',{}).items():
        if source:
            proof=current['devices'].get(slot) or original['devices'][slot]
            actual=file_proof(Path(proof['path']))
            require(all(actual[k]==proof[k] for k in ('guid','createtxg','bytes')),'source metadata staging identity changed')
            require(proof['guid']==item['guid'] and proof['createtxg']==item['createtxg'] and proof['bytes']==item['bytes'] and sha(proof['path'])==item['sha256'],'source snippet identity/data changed')
            current['devices'][slot]=proof
        else:
            proof=original['devices'][slot];path=Path(proof['path']);info=path.stat()
            require(path.resolve()==path and info.st_uid==0 and info.st_nlink==1 and str(info.st_dev)==proof['guid'] and str(info.st_ino)==proof['createtxg'] and info.st_size==proof['bytes'],'target snippet identity changed')
            current['devices'][slot]=proof
    if original.get('owned_metadata'):current['owned_metadata']=original['owned_metadata']
    return {'native':current}

def main(p):
    bind(p);action=p['action']
    if action=='snapshot':return snapshot(p)
    record=p['record'];p['migration_id']=record['plan']['migration_id'];p['plan']=record['plan']
    require(re.fullmatch('[0-9a-f]{64}',p['migration_id']),'invalid migration identity')
    if action=='fence':return fence(p,record)
    if action=='prepare_target':return prepare_target(p,record,p['manifest'])
    if action=='reopen':return reopen(p,record)
    if action=='seal':return seal(p,record,p['manifest'])
    if action=='verify_sealed':return verify_sealed(p,record)
    if action=='absence':return absence(p,record)
    if action=='cleanup_metadata':return cleanup_metadata(p,record)
    if action=='restored':return restored(p,record)
    if action=='activate':return activate(p,record)
    if action=='cleanup':return cleanup(p,record)
    if action=='abort':return cleanup(p,record,True)
    if action=='abort_unprepared':
        current=snapshot(p)
        require(current['config']==record['config_before'] and current['volumes']==record['volumes_before'],'unprepared target changed')
        api('set',p['vm_path']+'/config',digest=current['raw_config']['digest'],lock='migrate',onboot=0)
        return cleanup(p,record,True)
    raise RuntimeError('unsupported migration worker action')
if __name__=='__main__':
    parameters=json.loads(sys.stdin.read())
    signal.signal(signal.SIGTERM,stop_child);signal.signal(signal.SIGINT,stop_child)
    require(ctypes.CDLL(None).prctl(1,signal.SIGTERM,0,0,0)==0,'cannot bind migration adapter to helper lifetime')
    require(os.getppid()==parameters['helper_pid'],'migration helper exited before adapter initialization')
    print(json.dumps(main(parameters),sort_keys=True))
