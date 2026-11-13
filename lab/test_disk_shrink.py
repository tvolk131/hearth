import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch
import zlib

spec = importlib.util.spec_from_file_location("shrink_worker",Path(__file__).resolve().parents[1]/"src/control/shrink_worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class DiskShrinkTests(unittest.TestCase):
    def test_disk_tool_budget_scales_at_750_GiB_but_keeps_legacy_profile_and_hard_cap(self):
        self.assertEqual(worker.disk_tool_timeout(4096,3072),1200)
        self.assertEqual(worker.disk_tool_timeout(750*1024,500*1024),41800)
        self.assertEqual(worker.disk_tool_timeout(16*1024**2,15*1024**2),72*3600)
        for old,target in ((True,3072),(-1,3072),(16*1024**2+1,3072)):
            with self.assertRaises(RuntimeError):worker.disk_tool_timeout(old,target)

    def test_qemu_convert_and_compare_receive_disk_budget_while_metadata_stays_bounded(self):
        seen=[]
        class Process:
            returncode=0
            def communicate(self,data,timeout):seen.append(timeout);return "",""
        with patch.object(worker,"DISK_TOOL_TIMEOUT",41800),patch.object(worker.subprocess,"Popen",return_value=Process()):
            worker.run("qemu-img","convert","source","target")
            worker.run("qemu-img","compare","source","target")
            worker.run("qemu-img","info","source")
            worker.run("zpool","get","health","labpool")
        self.assertEqual(seen,[41800,41800,1200,1200])
    def test_unmount_failure_retains_probe_and_foreign_data(self):
        for foreign in (False,True):
            with self.subTest(foreign=foreign),tempfile.TemporaryDirectory() as directory:
                probe=Path(directory)/"probe";probe.mkdir()
                if foreign:(probe/"keep").write_text("preserve me")
                def run(*args,**kwargs):
                    if args[0]=="mount":return ""
                    if args[0]=="umount":raise RuntimeError("unmount failed")
                    if args[0]=="findmnt":return json.dumps({"filesystems":[{"source":"labpool/work","target":str(probe),"fstype":"zfs","options":"ro,nodev"}]})
                    raise AssertionError(args)
                with patch("tempfile.mkdtemp",return_value=str(probe)),patch.object(worker,"run",run),self.assertRaises(RuntimeError):
                    worker.empty_filesystem("labpool/work")
                self.assertTrue(probe.is_dir())
                if foreign:self.assertEqual((probe/"keep").read_text(),"preserve me")

    def test_ambiguous_mount_failure_never_recursively_removes_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            probe=Path(directory)/"probe";probe.mkdir();(probe/"keep").write_text("preserve me")
            with patch("tempfile.mkdtemp",return_value=str(probe)),patch.object(worker,"run",side_effect=RuntimeError("mount failed after uncertain effect")),self.assertRaises(RuntimeError):
                worker.empty_filesystem("labpool/work")
            self.assertEqual((probe/"keep").read_text(),"preserve me")

    def test_successful_setter_text_is_accepted_but_reads_remain_strict_JSON(self):
        with patch.object(worker,"run",return_value="update VM 9001: -lock migrate"):
            self.assertIsNone(worker.api("set","/nodes/pve-lab/qemu/9001/config",lock="migrate"))
            with self.assertRaises(json.JSONDecodeError):worker.api("get","/nodes/pve-lab/qemu/9001/config")

    def test_attachment_size_requires_one_exact_numeric_annotation(self):
        self.assertEqual(worker.attachment_size("ctl-alice:vm-9001-disk-0,size=3G"),3072*1024**2)
        self.assertEqual(worker.attachment_size("ctl-alice:vm-9001-disk-0,size=3072M"),3072*1024**2)
        self.assertEqual(worker.attachment_size("ctl-alice:vm-9001-disk-0,size=2.5G"),2560*1024**2)
        for suffix in ("",",size=3G,size=4G",",size=-3G",",size=nanG",",size=+3G"):
            with self.subTest(suffix=suffix),self.assertRaises(RuntimeError):worker.attachment_size("ctl-alice:vm-9001-disk-0"+suffix)

    def table(self):
        return {"label":"gpt","sectorsize":512,"id":"disk-id","partitions":[
            {"node":"/dev/fakep1","uuid":"bios","type":"21686148-6449-6e6f-744e-656564454649","start":2048,"size":2048},
            {"node":"/dev/fakep2","uuid":"root","type":"0fc63daf-8483-4772-8e79-3d69d8477de4","start":4096,"size":8000000}]}

    def inspect(self, table=None, filesystem="ext4", block_size=4096):
        table = table or self.table()
        def run(*args,**kwargs):
            if args[0]=="sfdisk":return json.dumps({"partitiontable":table})
            if args[0]=="blkid":return "root-fs-id" if "UUID" in args else (filesystem if args[-1]=="/dev/fakep2" else "")
            if args[0]=="dumpe2fs":return f"Block count: 1000000\nBlock size: {block_size}\nFree blocks: 500000\n"
            raise AssertionError(args)
        with patch.object(worker,"run",run),patch.object(worker,"unused",lambda disk:None):
            return worker.layout("/dev/fake")

    def test_known_layout_records_stable_identities_and_bounds(self):
        value,node=self.inspect()
        self.assertEqual(node,"/dev/fakep2")
        self.assertEqual(value["root_fs_uuid"],"root-fs-id")
        self.assertEqual(value["filesystem_bytes"],4096000000)
        self.assertEqual(value["used_bytes"],2048000000)
        self.assertNotIn("node",value["partitions"][0])

    def test_pinned_Debian_root_partition_GUID_is_accepted_and_preserved(self):
        table=self.table();table["partitions"][1]["type"]="4f68bce3-e8cd-4db1-96e7-fbcaf984b709"
        value,node=self.inspect(table)
        self.assertEqual(node,"/dev/fakep2")
        self.assertEqual(value["partitions"][1]["type"],table["partitions"][1]["type"])
        with self.assertRaises(RuntimeError):self.inspect(table,filesystem="xfs")

    def test_partition_number_uses_actual_GPT_slot_and_rejects_foreign_nodes(self):
        self.assertEqual(worker.partition_number("/dev/zd16","/dev/zd16p1"),1)
        self.assertEqual(worker.partition_number("/dev/zd16","/dev/zd16p14"),14)
        for node in ("/dev/zd17p1","/dev/zd16p0","/dev/zd16p01","/dev/zd16p129","/dev/zd16p1x"):
            with self.subTest(node=node),self.assertRaises(RuntimeError):worker.partition_number("/dev/zd16",node)

    def test_copy_backing_must_be_independent_thick_and_identity_stable(self):
        size=4096*1024**2
        value={"type":"volume","volsize":str(size),"refreservation":str(size+32*1024**2),"origin":"-","guid":"original-guid"}
        with patch.object(worker,"props",return_value=value):worker.same_volume("labpool/work/replacement",value,size)
        for field,replacement in (("type","filesystem"),("refreservation",str(size-1)),("origin","labpool/foreign@snapshot"),("volsize",str(size+1024**2))):
            changed=dict(value,**{field:replacement})
            with self.subTest(field=field),patch.object(worker,"props",return_value=changed),self.assertRaises(RuntimeError):
                worker.same_volume("labpool/work/replacement",changed,size)
        with patch.object(worker,"props",return_value=dict(value,guid="foreign-guid")),self.assertRaises(RuntimeError):
            worker.same_volume("labpool/work/replacement",value,size)

    def test_unsupported_filesystem_partition_type_sector_and_block_size_refused(self):
        for fs in ("xfs","crypto_LUKS","LVM2_member","ntfs"):
            with self.subTest(fs=fs),self.assertRaises(RuntimeError):self.inspect(filesystem=fs)
        for key,value in (("label","dos"),("sectorsize",4096)):
            table=self.table();table[key]=value
            with self.subTest(key=key),self.assertRaises(RuntimeError):self.inspect(table)
        table=self.table();table["partitions"][1]["type"]="e6d6d379-f507-44c2-a23c-238f2a3df928"
        with self.assertRaises(RuntimeError):self.inspect(table)
        with self.assertRaises(RuntimeError):self.inspect(block_size=1024)

    def test_root_must_be_last_by_extent(self):
        table=self.table();table["partitions"][0]["start"]=9000000
        with self.assertRaises(RuntimeError):self.inspect(table)

    def separate_boot(self, *, typed=True, selector=None, wrong_boot_uuid=False):
        table = {"label":"gpt", "sectorsize":512, "id":"disk-guid", "partitions":[
            {"node":"/dev/fakep1", "uuid":"boot-part", "type":worker.DATA_TYPE, "start":2048, "size":4096},
            {"node":"/dev/fakep14", "uuid":"root-part", "type":next(iter(worker.ROOT_TYPES)) if typed else worker.DATA_TYPE,
             "start":8192, "size":8000000}]}
        def run(*args, **kwargs):
            if args[0] == "sfdisk":return json.dumps({"partitiontable":table})
            if args[0] == "blkid":
                if "UUID" in args:return ("changed-boot" if wrong_boot_uuid else "boot-fs") if args[-1].endswith("p1") else "root-fs"
                return "ext4"
            if args[0] == "dumpe2fs":return "Block count: 1000000\nBlock size: 4096\nFree blocks: 500000\n"
            raise AssertionError(args)
        with patch.object(worker,"run",run),patch.object(worker,"unused",lambda disk:None):
            return worker.layout("/dev/fake", selector)

    def test_separate_boot_requires_typed_or_explicit_root_and_preserves_other_FS_identity(self):
        value,node=self.separate_boot()
        self.assertEqual(node,"/dev/fakep14")
        self.assertEqual(value["other_filesystems"],[{"partition_uuid":"boot-part","type":"ext4","uuid":"boot-fs"}])
        with self.assertRaisesRegex(RuntimeError,"explicit attested root UUID"):self.separate_boot(typed=False)
        selected,_=self.separate_boot(typed=False,selector="root-part")
        self.assertEqual(selected["root_partition_uuid"],"root-part")
        for typed in (False,True):
            with self.subTest(typed=typed),self.assertRaises(RuntimeError):self.separate_boot(typed=typed,selector="boot-part")
        changed,_=self.separate_boot(wrong_boot_uuid=True)
        with self.assertRaisesRegex(RuntimeError,"filesystem identity"):worker.preserve(value,changed,4096*1024**2)

    def test_layout_refuses_duplicate_partition_identity_and_foreign_partition_device(self):
        table=self.table();table["partitions"][0]["uuid"]="root"
        with self.assertRaises(RuntimeError):self.inspect(table)
        table=self.table();table["partitions"][1]["node"]="/dev/foreignp2"
        with self.assertRaises(RuntimeError):self.inspect(table)
        table=self.table();table["partitions"][0]["start"]=4096
        with self.assertRaises(RuntimeError):self.inspect(table)

    def test_nonroot_partition_byte_hashes_detect_boot_changes_and_ignore_root_data(self):
        with tempfile.TemporaryDirectory() as directory:
            disk=Path(directory)/"disk.raw";disk.write_bytes(bytes(512*100))
            value={"root_partition_uuid":"root", "partitions":[{"uuid":"boot","start":34,"size":8},
                {"uuid":"root","start":42,"size":58}]}
            with patch.object(worker,"unused",lambda disk:None):
                before=worker.partition_hashes(disk,value)
                with disk.open("r+b") as stream:stream.seek(42*512);stream.write(b"root changes")
                self.assertEqual(worker.partition_hashes(disk,value),before)
                with disk.open("r+b") as stream:stream.seek(34*512);stream.write(b"boot changes")
                self.assertNotEqual(worker.partition_hashes(disk,value),before)

    def trial_parameters(self):
        environment="0123456789abcdef0123456789abcdef"
        scope={"node":"pve1","pool":"rpool","root_dataset":"rpool/pve-dev-trial-"+environment,"root_guid":"99",
            "machine_id_sha256":hashlib.sha256(b"test-machine").hexdigest(),"storage_prefix":"trial-01234567-",
            "owners":["trial-admin","trial-user"],"vm_ids":[9001,9002],"capacity_mib":2*1024**2,"features":["resize"],"limits":{"disk_mib":1024**2,"budget_mib":2*1024**2,"min_memory_mib":1024,"memory_mib":2048,"cores":16}}
        old,target=750*1024,500*1024
        return {"environment":environment,"pool_guid":"42","physical_trial":{"pool_guid":"42","scope":scope},
            "owner":"trial-user","vm_id":9001,"old_mib":old,"target_mib":target,"preparation_id":"a"*64,
            "workspace_mib":old+(old+19)//20+target+(target+19)//20+64}

    def test_physical_scope_derives_large_disk_paths_and_refuses_scope_or_host_drift(self):
        expected=self.trial_parameters();scope=expected["physical_trial"]["scope"]
        def run(*args, **kwargs):
            if args[0]=="hostname":return "pve1"
            if args[:2]==("zpool","get"):return "ONLINE" if "health" in args else "42"
            if args[:2]==("zfs","get"):
                values={"type":"filesystem","guid":"99","org.pve-dev:trial":expected["environment"],
                    "quota":str(scope["capacity_mib"]*1024**2),"reservation":str(scope["capacity_mib"]*1024**2),"mountpoint":"none"}
                return "\n".join(k+"\t"+v for k,v in values.items())
            raise AssertionError(args)
        with patch.object(worker.os,"geteuid",return_value=0),patch.object(worker.Path,"read_bytes",return_value=b"test-machine"),patch.object(worker,"run",run):
            p=copy.deepcopy(expected);p["source_name"]="rpool/existing-vm";worker.configure_scope(p)
            self.assertEqual(p["source_name"],scope["root_dataset"]+"/control/trial-user/vm-9001-disk-0")
            self.assertEqual(p["workspace"],scope["root_dataset"]+"/shrink-"+"a"*64)
            for key,value in (("root_dataset","rpool/existing"),("node","pve2"),("root_guid","foreign"),
                              ("machine_id_sha256","0"*64),("vm_ids",[103]),("capacity_mib",32768)):
                p=copy.deepcopy(expected);p["physical_trial"]["scope"][key]=value
                with self.subTest(key=key),self.assertRaises(RuntimeError):worker.configure_scope(p)
            p=copy.deepcopy(expected);p["vm_id"]=103
            with self.assertRaises(RuntimeError):worker.configure_scope(p)

    def test_receipts_are_immutable_bound_private_and_reject_aliases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve()/"receipts"
            def private(path,create=False):
                if create:path.mkdir(mode=0o700,exist_ok=True)
                worker.require(path.resolve(strict=True)==path and not path.is_symlink(),"unsafe test receipt path")
            native_fstat=os.fstat
            def root_fstat(fd):
                values=list(native_fstat(fd));values[4]=0;return os.stat_result(values)
            p=self.trial_parameters();p.update(source_name="rpool/source",workspace="rpool/work")
            with patch.object(worker,"RECEIPT_ROOT",root),patch.object(worker,"_private_directory",private),patch.object(worker.os,"fstat",root_fstat):
                identity=worker.write_receipt(p,"baseline",{"source_guid":"known"},create=True)
                self.assertEqual(worker.read_receipt(p,"baseline"),{"source_guid":"known"})
                self.assertEqual(hashlib.sha256(Path(identity["path"]).read_bytes()).hexdigest(),identity["sha256"])
                with self.assertRaises(FileExistsError):worker.write_receipt(p,"baseline",{"source_guid":"foreign"})
                with self.assertRaises(FileExistsError):worker.write_receipt(p,"baseline",{},create=True)
                changed=dict(p,owner="trial-admin")
                with self.assertRaises(RuntimeError):worker.read_receipt(changed,"baseline")
                finishing=dict(p,operation_id="b"*64)
                worker.write_receipt(finishing,"finish",{"recovery_removed":True})
                with self.assertRaises(RuntimeError):worker.read_receipt(dict(finishing,operation_id="c"*64),"finish")
                path=Path(identity["path"]);path.chmod(0o644)
                with self.assertRaises(RuntimeError):worker.read_receipt(p,"baseline")
                path.chmod(0o600);os.link(path,path.with_name("alias"))
                with self.assertRaises(RuntimeError):worker.read_receipt(p,"baseline")
                path.with_name("alias").unlink()
                path.with_name("stage.json").symlink_to(path)
                with self.assertRaises(OSError):worker.read_receipt(p,"stage")

    def test_abort_never_mutates_after_ambiguous_or_postswap_inspection(self):
        for state in ("ambiguous","swapped","unlocked","finished"):
            with self.subTest(state=state),patch.object(worker,"inspect",return_value={"state":state,"abort_safe":False}),patch.object(worker,"run") as run:
                with self.assertRaises(RuntimeError):worker.abort({})
                run.assert_not_called()

    def test_quick_inspection_only_offers_candidates_without_reading_disk_bytes(self):
        source={key:"-" for key in worker.PROPS}
        source.update(type="volume",guid="source-guid",createtxg="3",volsize=str(4096*1024**2),
                      refreservation=str(4200*1024**2),readonly="off")
        p={"scope_root":"labpool","source_name":"labpool/control/alice/vm-9001-disk-0",
           "workspace":"labpool/work","preparation_id":"a"*64,"vm_path":"/nodes/pve-lab/qemu/9001","quick":True}
        baseline={"source":source,"source_sha256":"b"*64,"original_config":{"protection":1}}
        def receipt(p,name,optional=False,with_meta=False):return baseline if name=="baseline" else None
        with patch.object(worker,"read_receipt",receipt),patch.object(worker,"props",return_value=source),\
                patch.object(worker,"names",side_effect=lambda name:[name]),patch.object(worker,"config",return_value={}),\
                patch.object(worker,"sha",side_effect=AssertionError("full disk hash")),\
                patch.object(worker,"device",side_effect=AssertionError("raw device probe")):
            inspected=worker.inspect(p)
        self.assertEqual(inspected["classification"],"full_verification_required")
        self.assertTrue(inspected["abort_candidate"])
        self.assertFalse(inspected["abort_safe"])
        self.assertFalse(inspected["bytes_verified"])
        self.assertEqual(inspected["verified_phases"],{})

    def test_partition_identity_and_other_partition_changes_are_refused(self):
        before,_=self.inspect();after=copy.deepcopy(before)
        after["root_size"]=6000000;after["partitions"][1]["size"]=6000000;after["filesystem_bytes"]=3000000000
        target=3072*1024**2
        worker.preserve(before,after,target)
        for field in ("disk_guid","root_partition_uuid","root_fs_uuid","root_start"):
            changed=copy.deepcopy(after);changed[field]="changed"
            with self.subTest(field=field),self.assertRaises(RuntimeError):worker.preserve(before,changed,target)
        changed=copy.deepcopy(after);changed["partitions"][0]["size"]-=1
        with self.assertRaises(RuntimeError):worker.preserve(before,changed,target)
        changed=copy.deepcopy(after);changed["filesystem_bytes"]=target
        with self.assertRaises(RuntimeError):worker.preserve(before,changed,target)

    def test_root_may_grow_to_the_last_usable_sector_after_guest_boot(self):
        before,_=self.inspect();after=copy.deepcopy(before);target=3072*1024**2
        after["root_size"]=target//512-33-after["root_start"]
        after["partitions"][1]["size"]=after["root_size"]
        after["filesystem_bytes"]=after["root_size"]*512//4096*4096
        worker.preserve(before,after,target)
        after["root_size"]+=1
        with self.assertRaises(RuntimeError):worker.preserve(before,after,target)

    def gpt(self, path, *, size=4*1024**2, wrong_alternate=False, bounds=None, backup_bounds=None, partitions=(), backup_guid=None, entry_lba=None, locations=None):
        last=size//512-1;entries=bytearray(128*128)
        for i,(start,end) in enumerate(partitions):
            entries[i*128:i*128+16]=bytes([1])*16
            entries[i*128+16:i*128+32]=bytes([i+1])*16
            struct.pack_into("<2Q",entries,i*128+32,start,end)
        with path.open("wb") as out:
            out.truncate(size)
            primary,backup=locations or (2,last-32)
            for own,other,lba in ((1,last,primary),(last,1,backup)):
                header=bytearray(512);header[:8]=b"EFI PART"
                struct.pack_into("<III",header,8,0x10000,92,0)
                first,end=(backup_bounds if own==last and backup_bounds else bounds) or (34,last-33)
                struct.pack_into("<4Q",header,24,own,other-1 if wrong_alternate else other,first,end)
                header[56:72]=backup_guid if own==last and backup_guid else bytes([7])*16
                struct.pack_into("<QIII",header,72,lba if entry_lba is None else entry_lba,128,128,zlib.crc32(entries)&0xffffffff)
                struct.pack_into("<I",header,16,zlib.crc32(header[:92])&0xffffffff)
                out.seek(own*512);out.write(header);out.seek(lba*512);out.write(entries)
        return size

    def test_750_GiB_GPT_geometry_validates_full_64_bit_CRC_and_bounds_without_allocating_disk(self):
        class VirtualDisk:
            def __init__(self):self.offset=0;self.extents={};self.read_bytes=0
            def open(self,*args):return self
            def __enter__(self):return self
            def __exit__(self,*args):pass
            def truncate(self,size):self.size=size
            def seek(self,offset):self.offset=offset
            def write(self,data):self.extents[self.offset]=bytes(data);self.offset+=len(data)
            def read(self,count):
                result=bytearray(count)
                for start,data in self.extents.items():
                    lo=max(start,self.offset);hi=min(start+len(data),self.offset+count)
                    if lo<hi:result[lo-self.offset:hi-self.offset]=data[lo-start:hi-start]
                self.offset+=count;self.read_bytes+=count;return bytes(result)
        size=750*1024**3;last=size//512-1
        disk=VirtualDisk();self.gpt(disk,size=size,partitions=((2048,4095),(4096,1052671),(1052672,last-33)))
        with patch("builtins.open",return_value=disk):worker.verify_gpt("virtual750GiB",size)
        self.assertLess(disk.read_bytes,40*1024)
        for kwargs in ({"partitions":((1052672,last-32),)},{"wrong_alternate":True},{"backup_bounds":(34,last-34)}):
            disk=VirtualDisk();self.gpt(disk,size=size,**kwargs)
            with self.subTest(kwargs=kwargs),patch("builtins.open",return_value=disk),self.assertRaises(RuntimeError):worker.verify_gpt("virtual750GiB",size)
        disk=VirtualDisk();self.gpt(disk,size=size)
        broken=bytearray(disk.extents[last*512]);broken[16]^=1;disk.extents[last*512]=bytes(broken)
        with patch("builtins.open",return_value=disk),self.assertRaisesRegex(RuntimeError,"CRC"):worker.verify_gpt("virtual750GiB",size)

    def test_GPT_arrays_may_use_reserved_gaps_but_never_usable_data_sectors(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"gpt.raw"
            size=self.gpt(path,bounds=(2048,6000),locations=(8,6144),partitions=((2048,4095),(4096,5999)))
            worker.verify_gpt(path,size)
            for locations in ((3,8159),(2,8151)):
                with self.subTest(locations=locations):
                    size=self.gpt(path,locations=locations)
                    with self.assertRaises(RuntimeError):worker.verify_gpt(path,size)

    def test_GPT_allows_vendor_reserved_gaps_but_refuses_valid_CRC_invalid_extents(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"gpt.raw"
            size=self.gpt(path,bounds=(2048,6144),partitions=((2048,4095),(4096,6000)))
            worker.verify_gpt(path,size)
            for kwargs in (
                {"bounds":(33,6144)}, {"bounds":(6144,2048)}, {"bounds":(2048,8190)},
                {"bounds":(2048,6144),"backup_bounds":(2048,6100)},
                {"backup_guid":bytes([8])*16}, {"entry_lba":3},
                {"bounds":(2048,6144),"partitions":((2047,4095),)},
                {"bounds":(2048,6144),"partitions":((2048,6145),)},
                {"partitions":((2048,4096),(4096,6000))},
                {"partitions":((4096,2048),)},
            ):
                with self.subTest(kwargs=kwargs):
                    size=self.gpt(path,**kwargs)
                    with self.assertRaises(RuntimeError):worker.verify_gpt(path,size)

    def test_both_GPT_checksums_and_cross_references_are_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"toy-gpt.raw";size=self.gpt(path)
            worker.verify_gpt(path,size)
            with path.open("r+b") as out:out.seek(size-512+16);out.write(b"broken")
            with self.assertRaises(RuntimeError):worker.verify_gpt(path,size)
            size=self.gpt(path,wrong_alternate=True)
            with self.assertRaises(RuntimeError):worker.verify_gpt(path,size)
            size=self.gpt(path)
            with path.open("r+b") as out:out.seek(2*512);out.write(b"changed entries")
            with self.assertRaises(RuntimeError):worker.verify_gpt(path,size)

    def test_foreign_current_and_pending_root_or_recovery_references_are_refused(self):
        p={"vm_id":9001,"workspace":"labpool/pve-dev-shrink-operation"}
        for reference in ("ctl-alice:vm-9001-disk-0",p["workspace"]+"/original"):
            def api(method,path,**args):
                if path=="/cluster/resources":return [{"type":"qemu","node":"pve-lab","vmid":9002}]
                return [{"pending":reference}] if path.endswith("/pending") else {}
            with self.subTest(reference=reference),patch.object(worker,"api",api),self.assertRaises(RuntimeError):worker.foreign_references(p)


if __name__=="__main__":unittest.main()
