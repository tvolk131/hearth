"""Read-only/unit contracts for the fixed migration effect adapter."""
import copy
import importlib.util
import pathlib
import unittest
from unittest import mock

spec=importlib.util.spec_from_file_location('migration_worker',pathlib.Path(__file__).resolve().parents[1]/'src/control/migration_worker.py')
worker=importlib.util.module_from_spec(spec);spec.loader.exec_module(worker)

class MigrationAdapterTests(unittest.TestCase):
    def params(self):
        return {'binding':{'environment':'1'*32,'pool_guid':'42','physical_trial':{'scope':{'storage_prefix':'trial-11111111-'}}},'node':'pve2','storage':'trial-11111111-trial-user','state_dir':'/var/lib/pve-dev-trial-'+('1'*32)+'/helper','migration_id':'a'*64,'owner':'trial-user','vm_id':9002,'plan':{'name':'dev-test','target_bridge':'vmbr77'}}
    def source_config(self):
        return {'name':'dev-test','description':'source marker','smbios1':'uuid=source','vmgenid':'source-genid','scsi0':'source-storage:vm-9001-disk-0,size=4G','ide2':'source-storage:vm-9001-cloudinit,media=cdrom,size=4M','net0':'virtio=02:50:00:00:00:01,bridge=vmbr77','ipconfig0':'ip=10.77.0.11/24,gw=10.77.0.1','protection':'1','meta':'creation-qemu=11.1.3,ctime=1'}
    def snapshot_fixture(self):
        p=self.params();p.update(disk_mib=2048,vm_path='/nodes/pve2/qemu/9002',volume_names={'root':'pool/trial/control/trial-user/vm-9002-disk-0','cloudinit':'pool/trial/control/trial-user/vm-9002-cloudinit'})
        owner_marker='pve-dev-control:'+p['binding']['environment']+':'+p['owner']
        config=self.source_config();config.update(description=owner_marker+':9002',scsi0=p['storage']+':vm-9002-disk-0,size=2G',ide2=p['storage']+':vm-9002-cloudinit,media=cdrom,size=4M',digest='native-digest')
        # These are the actual asymmetric Host creation contracts: root gets
        # vm_marker(), while PVE's cloud-init allocation inherits owner_marker().
        volumes={}
        for i,(slot,name) in enumerate(p['volume_names'].items()):
            size=(p['disk_mib'] if slot=='root' else 4)*worker.MIB
            volumes[name]={'type':'volume','guid':str(i+1),'createtxg':str(i+10),'volsize':str(size),'refreservation':str(size+worker.MIB),'origin':'-','org.pve-dev:owner':owner_marker+':9002' if slot=='root' else owner_marker,'org.pve-dev:image':'c'*64 if slot=='root' else '-','org.pve-dev:migration':'-','readonly':'off'}
        return p,config,volumes
    def native_snapshot(self,p,config,volumes):
        def api(method,path,**values):
            self.assertEqual(method,'get','snapshot must never mutate Proxmox state')
            if path==p['vm_path']+'/config':return copy.deepcopy(config)
            if path==p['vm_path']+'/status/current':return {'status':'stopped'}
            if path=='/cluster/ha/resources':return []
            if path=='/cluster/resources':return [{'type':'qemu','node':p['node'],'vmid':p['vm_id']}]
            self.fail('unexpected native API read '+path)
        def device(name,size):return {'path':'/dev/zd'+str(list(volumes).index(name)),'bytes':size,'rdev':100+list(volumes).index(name)}
        with mock.patch.object(worker,'api',side_effect=api),mock.patch.object(worker,'names',side_effect=lambda name:[name]),mock.patch.object(worker,'props',side_effect=lambda name:copy.deepcopy(volumes[name])),mock.patch.object(worker,'volume_device',side_effect=device) as opened,mock.patch.object(pathlib.Path,'exists',return_value=False),mock.patch.object(pathlib.Path,'resolve',lambda path,strict=False:path),mock.patch.object(worker,'sha',return_value='d'*64),mock.patch.object(worker,'run',return_value='instance-id: original-vm-9002') as run:
            result=worker.snapshot(p)
        self.assertEqual(opened.call_count,2)
        run.assert_called_once_with('qm','cloudinit','dump','9002','meta')
        return result
    def test_native_snapshot_accepts_helper_root_and_inherited_cloudinit_markers(self):
        p,config,volumes=self.snapshot_fixture()
        for protection in ('1',1,True):
            with self.subTest(protection=protection):
                config['protection']=protection
                result=self.native_snapshot(p,config,volumes)
                self.assertEqual(result['volumes']['root']['org.pve-dev:owner'],'pve-dev-control:'+p['binding']['environment']+':trial-user:9002')
                self.assertEqual(result['volumes']['cloudinit']['org.pve-dev:owner'],'pve-dev-control:'+p['binding']['environment']+':trial-user')
                self.assertEqual(result['devices']['root']['bytes'],2048*worker.MIB)
                self.assertEqual(result['devices']['cloudinit']['bytes'],4*worker.MIB)
                self.assertEqual(result['generated_meta'],'instance-id: original-vm-9002\n')
    def test_native_snapshot_refuses_foreign_root_or_cloudinit_claims(self):
        p,config,volumes=self.snapshot_fixture();owner='pve-dev-control:'+p['binding']['environment']+':trial-user'
        for slot,foreign in [('root',owner),('root',owner+':9001'),('root','pve-dev-control:'+('2'*32)+':trial-user:9002'),('cloudinit',owner+':9002'),('cloudinit','pve-dev-control:'+p['binding']['environment']+':another-user')]:
            with self.subTest(slot=slot,marker=foreign):
                changed=copy.deepcopy(volumes);changed[p['volume_names'][slot]]['org.pve-dev:owner']=foreign
                with self.assertRaisesRegex(RuntimeError,'volume ownership changed for '+slot):self.native_snapshot(p,config,changed)
    def test_cloudinit_annotation_normalization_preserves_every_other_field(self):
        a=self.source_config();b=copy.deepcopy(a);b['ide2']=b['ide2'].removesuffix(',size=4M');b['digest']='new';b['lock']='migrate'
        self.assertEqual(worker.normalized(a),worker.normalized(b))
        b['ipconfig0']='ip=10.77.0.12/24,gw=10.77.0.1'
        self.assertNotEqual(worker.normalized(a),worker.normalized(b))
        self.assertIn('size=4G',worker.normalized(a)['scsi0'])
    def test_boolean_numeric_and_current_memory_values_are_canonical(self):
        self.assertEqual(worker.normalized({'kvm':False,'memory':'current=1024','description':' marker \n'}),{'kvm':'0','memory':'1024','description':'marker'})
    def test_target_mapping_preserves_guest_uuid_mac_and_instance_dependencies(self):
        p=self.params();source=self.source_config()
        value=worker.target_config(p,{'config':source,'snippets':{}},{'meta':'creation-qemu=11.1.3,ctime=2'})
        self.assertEqual(value['smbios1'],source['smbios1']);self.assertEqual(value['vmgenid'],source['vmgenid']);self.assertEqual(value['net0'],source['net0']);self.assertEqual(value['ipconfig0'],source['ipconfig0'])
        self.assertEqual(value['scsi0'],p['storage']+':vm-9002-disk-0,size=4G')
        self.assertEqual(value['ide2'],p['storage']+':vm-9002-cloudinit,media=cdrom')
        self.assertEqual(value['onboot'],'0');self.assertEqual(value['protection'],'1')
        self.assertEqual(value['meta'],'creation-qemu=11.1.3,ctime=2')
    def test_unknown_hooks_passthrough_or_unmodeled_network_settings_refused(self):
        for key in ('args','hookscript','hostpci0','usb0'):
            config=self.source_config();config[key]='arbitrary'
            with self.subTest(key=key),self.assertRaises(RuntimeError):worker.target_config(self.params(),{'config':config,'snippets':{}},{})
        config=self.source_config();config['net0']+=',tag=4'
        with self.assertRaises(RuntimeError):worker.target_config(self.params(),{'config':config,'snippets':{}},{})
    def test_custom_snippets_outside_owned_storage_refused_before_file_access(self):
        for ref in ('meta=local:snippets/prod.yaml','meta=trial-11111111-meta:snippets/../../prod.yaml','vendor=trial-11111111-meta:snippets/a.yaml'):
            with self.subTest(ref=ref),mock.patch.object(worker,'api') as api,self.assertRaises(RuntimeError):
                worker.snippet_sources(self.params(),{'cicustom':ref})
            api.assert_not_called()
    def test_missing_snippet_manifest_cannot_silently_regenerate_cloudinit(self):
        config=self.source_config();config['cicustom']='meta=trial-11111111-meta:snippets/meta.yaml'
        with self.assertRaises(RuntimeError):worker.target_config(self.params(),{'config':config,'snippets':{}},{})
    def test_metadata_storage_uses_real_api_wrapper_without_route_path_collision(self):
        p=self.params();manifest={'config':self.source_config(),'snippets':{'meta':{'bytes':54,'sha256':'a'*64}}}
        with mock.patch.object(worker,'run',side_effect=['[]','null']) as run:
            target=worker.target_config(p,manifest,{})
        self.assertEqual(run.call_args_list,[
            mock.call('pvesh','get','/storage','--output-format','json'),
            mock.call('pvesh','create','/storage','--output-format','json','--storage',worker.metadata_storage(p),'--type','dir','--path',str(worker.metadata_base(p)),'--content','snippets','--nodes',p['node'])])
        self.assertEqual(target['cicustom'],'meta='+worker.metadata_storage(p)+':snippets/meta.yaml')
    def test_native_command_failure_retains_bounded_useful_stderr(self):
        process=mock.Mock();process.returncode=23;process.communicate.return_value=('',('x'*5000)+'specific native failure')
        with mock.patch.object(worker.subprocess,'Popen',return_value=process):
            with self.assertRaisesRegex(RuntimeError,'pvesh failed \\(exit 23\\).*specific native failure') as error:worker.run('pvesh','create','/storage')
        self.assertLess(len(str(error.exception)),1700)
        self.assertIsNone(worker.child,'an already reaped failed command must not remain an active child')
    def test_generated_source_metadata_is_transferred_as_an_immutable_dependency(self):
        p=self.params();p.update(plan_sha256='b'*64,volume_names={'root':'pool/own/root','cloudinit':'pool/own/seed'},vm_path='/nodes/source/qemu/9001')
        meta='instance-id: unchanged-source-instance\nlocal-hostname: dev-test\n'
        config=worker.normalized(self.source_config());volumes={slot:{'guid':str(i+1),'createtxg':str(i+10)} for i,slot in enumerate(p['volume_names'])}
        state={'config':config,'raw_config':{'digest':'digest'},'volumes':volumes,'devices':{slot:{'path':'/dev/zd'+str(i),'bytes':4*1024**2} for i,slot in enumerate(volumes)},'snippets':{},'generated_meta':meta}
        proof={'path':'/private/meta.yaml','bytes':len(meta.encode()),'sha256':'c'*64,'guid':'3','createtxg':'30','regular':True}
        owned={'identity':{},'files':{'meta':proof}}
        record={'config_before':config,'volumes_before':volumes,'native':{'generated_meta':meta}}
        with mock.patch.object(worker,'snapshot',return_value=copy.deepcopy(state)),mock.patch.object(worker,'stable',return_value=copy.deepcopy(state)),mock.patch.object(worker,'api'),mock.patch.object(worker,'run'),mock.patch.object(worker,'sha',return_value='d'*64),mock.patch.object(worker,'allocate_metadata',return_value=owned) as allocate:
            value=worker.fence(p,record)
        allocate.assert_called_once_with(p,{'meta':meta.encode()})
        self.assertEqual(value['manifest']['snippets']['meta'],{k:proof[k] for k in ('bytes','sha256','guid','createtxg')})
        self.assertEqual(value['native']['devices']['meta'],proof)
        with mock.patch.object(worker,'api',side_effect=[[],None]) as api:
            target=worker.target_config(p,value['manifest'],{})
        self.assertEqual(target['cicustom'],'meta='+worker.metadata_storage(p)+':snippets/meta.yaml')
        self.assertEqual(api.call_args.kwargs['path'],str(worker.metadata_base(p)))
    def test_absence_requires_original_guids_globally_absent_and_no_active_vm_task(self):
        p=self.params();p['volume_names']={'root':'pool/own/root'}
        record={'volumes_before':{'root':{'guid':'123'}}}
        with mock.patch.object(pathlib.Path,'exists',return_value=False),mock.patch.object(worker,'api',return_value=[]),mock.patch.object(worker,'run',return_value='pool/renamed-original\t123'),self.assertRaises(RuntimeError):worker.absence(p,record)
        with mock.patch.object(pathlib.Path,'exists',return_value=False),mock.patch.object(worker,'api',return_value=[{'id':'9002'}]),mock.patch.object(worker,'run') as run,self.assertRaises(RuntimeError):worker.absence(p,record)
        run.assert_not_called()
        with mock.patch.object(pathlib.Path,'exists',return_value=False),mock.patch.object(worker,'api',return_value=[]),mock.patch.object(worker,'run',return_value='pool/unrelated\t456') as run:
            self.assertTrue(worker.absence(p,record)['volumes_absent'])
        self.assertTrue(all(command.args[0]=='zfs' for command in run.call_args_list),'absence reconciliation must not submit native destruction')
    def test_metadata_cleanup_requires_absence_before_accessing_any_file(self):
        p=self.params();record={'native':{'owned_metadata':{'identity':{},'files':{}}}}
        with mock.patch.object(worker,'absence',side_effect=RuntimeError('disk remains')),mock.patch.object(worker,'file_proof') as proof,mock.patch.object(worker,'api') as api,self.assertRaises(RuntimeError):worker.cleanup_metadata(p,record)
        proof.assert_not_called();api.assert_not_called()
    def test_cleanup_keeps_source_fenced_and_only_destroys_exact_owned_vm(self):
        p=self.params();p['volume_names']={'root':'pool/own/vm-9002-disk-0','cloudinit':'pool/own/vm-9002-cloudinit'};p['vm_path']='/nodes/pve2/qemu/9002'
        config=worker.normalized(self.source_config());config['onboot']='0'
        volumes={k:{'guid':str(i+1),'createtxg':str(i+10),'volsize':'4194304','refreservation':'6291456','readonly':'on'} for i,k in enumerate(p['volume_names'])}
        state={'config':config,'raw_config':{'digest':'digest','lock':'migrate'},'volumes':volumes,'devices':{k:{'path':'/dev/zd'+str(i)} for i,k in enumerate(volumes)}}
        record={'role':'source','config_before':config,'volumes_before':volumes,'manifest':{'volumes':{k:{'sha256':'b'*64} for k in volumes}}}
        commands=[]
        with mock.patch.object(worker,'snapshot',return_value=state),mock.patch.object(worker,'sha',return_value='b'*64),mock.patch.object(worker,'api') as api,mock.patch.object(worker,'run',side_effect=lambda *args,**kw:commands.append(args) or ''),mock.patch.object(pathlib.Path,'exists',return_value=False):
            self.assertTrue(worker.cleanup(p,record)['cleaned'])
        self.assertIn(('qm','destroy','9002','--skiplock','1','--purge','0','--destroy-unreferenced-disks','0'),commands)
        self.assertFalse(any('unlock' in command for command in commands))
        self.assertTrue(any(call.kwargs.get('protection')==0 and 'delete' not in call.kwargs for call in api.call_args_list))
    def test_cleanup_refuses_changed_identity_before_any_mutation(self):
        p=self.params();p['vm_path']='/nodes/pve2/qemu/9002';p['volume_names']={'root':'pool/own/root'}
        cfg={'onboot':'0'};current={'config':cfg,'raw_config':{'lock':'migrate'},'volumes':{'root':{'guid':'changed','createtxg':'1','volsize':'4194304','refreservation':'6291456'}}}
        record={'role':'source','config_before':cfg,'volumes_before':{'root':{'guid':'original','createtxg':'1','volsize':'4194304','refreservation':'6291456'}}}
        with mock.patch.object(worker,'snapshot',return_value=current),mock.patch.object(worker,'api') as api,mock.patch.object(worker,'run') as run,self.assertRaises(RuntimeError):worker.cleanup(p,record)
        api.assert_not_called();run.assert_not_called()

if __name__=='__main__':unittest.main()
