import copy
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import types
import unittest
from unittest.mock import patch
import zlib

ROOT = Path(__file__).resolve().parents[1]
worker = types.ModuleType("growth_adapter_tests")
exec((ROOT / "src/control/shrink_worker.py").read_text(), worker.__dict__)
exec((ROOT / "src/control/grow_worker.py").read_text(), worker.__dict__)


class FakeGrowth:
    def __init__(self):
        self.old, self.target = 4096, 8192
        self.volume = {"type":"volume", "guid":"original-guid", "createtxg":"7", "volsize":str(self.old*worker.MIB),
            "refreservation":str((self.old+64)*worker.MIB), "origin":"-", "readonly":"off", "org.pve-dev:owner":"alice",
            "org.pve-dev:image":"image-guid", "org.pve-dev:shrink":"-"}
        self.marker = "-"
        sectors = self.old*worker.MIB//512-2048-262144
        self.layout = {"disk_guid":"disk-guid", "root_partition_uuid":"root-part", "root_fs_uuid":"root-fs",
            "root_start":262144, "root_size":sectors, "filesystem_bytes":sectors*512//4096*4096, "used_bytes":512*worker.MIB,
            "other_filesystems":[{"partition_uuid":"boot-part","type":"ext4","uuid":"boot-fs"}],
            "partitions":[{"uuid":"boot-part","type":worker.DATA_TYPE,"start":2048,"size":131072},
                {"uuid":"root-part","type":next(iter(worker.ROOT_TYPES)),"start":262144,"size":sectors}]}
        self.span = self.old*worker.MIB
        self.cfg = {"digest":"before", "protection":1, "scsi0":"ctl-alice:vm-9001-disk-0,size=4G", "name":"guest", "cores":2}
        self.receipts = {}
        self.mutations = []
        self.boot_hash = "boot-unchanged"
        self.p = {"environment":"a"*32, "operation_id":"b"*64, "pool_guid":"42", "owner":"alice", "vm_id":9001,
            "old_mib":self.old, "target_mib":self.target, "source_name":"labpool/control/alice/vm-9001-disk-0",
            "scope_root":"labpool", "pool":"labpool", "workspace":"labpool/pve-dev-shrink-"+"b"*64,
            "vm_path":"/nodes/pve-lab/qemu/9001", "source":copy.deepcopy(self.volume), "config":copy.deepcopy(self.cfg)}

    def receipt_meta(self, name, value):
        return {"path":"/fixed/"+name+".json", "sha256":hashlib.sha256(json.dumps(value,sort_keys=True).encode()).hexdigest()}

    def write(self, p, name, value, create=False):
        if name in self.receipts or (create and self.receipts):raise FileExistsError(name)
        self.receipts[name] = copy.deepcopy(value)
        return self.receipt_meta(name,value)

    def read(self, p, name, optional=False, with_meta=False):
        if name not in self.receipts:
            if optional:return None
            raise FileNotFoundError(name)
        value=copy.deepcopy(self.receipts[name])
        return {"value":value,"receipt":self.receipt_meta(name,value)} if with_meta else value

    def api(self, method, path, **values):
        if method == "get":
            if path.endswith("/config"):return copy.deepcopy(self.cfg)
            if path.endswith("/status/current"):return {"status":"stopped"}
            if path.endswith(("/snapshot","/pending")):return []
            raise AssertionError(path)
        self.mutations.append(("api",copy.deepcopy(values)))
        if "lock" in values:self.cfg["lock"]=values["lock"]
        if values.get("delete")=="lock":self.cfg.pop("lock",None)
        if "scsi0" in values:self.cfg["scsi0"]=values["scsi0"]
        self.cfg["digest"]="changed"

    def run(self, *args, **kwargs):
        if (args[0] in ("sgdisk","sfdisk","resize2fs") or (args[0]=="e2fsck" and "-n" not in args)
                or args[:2] in (("zfs","set"),("zfs","inherit"))):
            self.mutations.append(args)
        if args[:2] == ("zfs","set"):
            for entry in args[2:-1]:
                key,value=entry.split("=",1)
                if key=="volsize":self.volume["volsize"]=value
                elif key=="refreservation":self.volume["refreservation"]=str(int(self.volume["volsize"])+64*worker.MIB)
                elif key=="org.pve-dev:grow":self.marker=value
                else:raise AssertionError(args)
            return ""
        if args[:2] == ("zfs","inherit"):
            self.marker="-";return ""
        if args[:2] == ("zfs","get"):return self.marker
        if args[0] == "blockdev":return self.volume["volsize"]
        if args[0] == "sgdisk":
            if args[1] == "--move-second-header":self.span=int(self.volume["volsize"])
            return ""
        if args[0] == "sfdisk":
            assert args[-2]=="14",args
            sectors=int(kwargs["data"].removeprefix("size=").strip())
            self.layout["root_size"]=sectors;self.layout["partitions"][1]["size"]=sectors
            return ""
        if args[0] == "resize2fs":
            self.layout["filesystem_bytes"]=self.layout["root_size"]*512//4096*4096
            return ""
        if args[0] in ("e2fsck","zpool"):return ""
        raise AssertionError(args)

    def patch(self):
        def span(disk,size):
            worker.require(self.span <= size,"GPT span outside volume")
            return self.span
        def verify(disk,size):worker.require(self.span==size,"mixed or stale GPT")
        return patch.multiple(worker,run=self.run,api=self.api,props=lambda name:copy.deepcopy(self.volume),
            names=lambda name:[name],device=lambda name:"/dev/zd0",unused=lambda disk:None,
            layout=lambda disk,selector=None:(copy.deepcopy(self.layout),"/dev/zd0p14"),
            foreign_references=lambda p:None,partition_hashes=lambda disk,layout:{"boot-part":self.boot_hash},
            declared_gpt_size=span,verify_gpt=verify,write_receipt=self.write,read_receipt=self.read)

    def prepare(self):
        proof=worker.grow_prepare(self.p);self.p["baseline"]=proof;return proof

    def partitioned(self):
        self.prepare();worker.grow_volume(self.p);worker.grow_partition(self.p)


class DiskGrowthTests(unittest.TestCase):
    def test_full_grow_preserves_identity_boot_bytes_and_fence_until_final_unlock(self):
        f=FakeGrowth();original=copy.deepcopy(f.layout)
        with f.patch():
            f.prepare()
            self.assertEqual(f.cfg["lock"],"migrate");self.assertEqual(f.marker,f.p["operation_id"])
            worker.grow_volume(f.p)
            self.assertEqual(f.layout,original)
            worker.grow_partition(f.p)
            self.assertEqual(f.layout["filesystem_bytes"],original["filesystem_bytes"])
            self.assertEqual(f.layout["partitions"][0],original["partitions"][0])
            worker.grow_filesystem(f.p)
            self.assertIn("lock",f.cfg)
            inspected=worker.grow_inspect(f.p)
            self.assertEqual(inspected["state"],"filesystem_extended")
            self.assertIn("grow_filesystem",inspected["verified_phases"])
            self.assertFalse(inspected["abort_safe"])
            proof=worker.grow_unlock(f.p)
            self.assertTrue(proof["grown"])
            self.assertNotIn("lock",f.cfg);self.assertEqual(f.marker,"-")
            self.assertEqual(f.volume["guid"],"original-guid")
            self.assertEqual(f.layout["root_fs_uuid"],"root-fs")
            inspected=worker.grow_inspect(f.p)
            self.assertEqual(inspected["state"],"unlocked")
            self.assertEqual(inspected["verified_phases"]["grow_unlock"],proof)

    def test_lost_volume_reply_can_be_inspected_but_volume_command_is_not_replayed(self):
        f=FakeGrowth()
        with f.patch():
            f.prepare();proof=worker.grow_volume(f.p)
            inspected=worker.grow_inspect(f.p)
            self.assertEqual(inspected["state"],"volume_extended")
            self.assertEqual(inspected["verified_phases"]["grow_volume"],proof)
            before=list(f.mutations)
            with self.assertRaises(RuntimeError):worker.grow_volume(f.p)
            self.assertEqual(f.mutations,before)

    def test_effect_without_completion_receipt_is_observed_without_fabricating_verified_phase(self):
        f=FakeGrowth()
        with f.patch():
            f.prepare();native=f.write
            def interrupted(p,name,value,create=False):
                if name=="grow-volume":raise RuntimeError("power loss before receipt")
                return native(p,name,value,create)
            with patch.object(worker,"write_receipt",interrupted),self.assertRaises(RuntimeError):worker.grow_volume(f.p)
            inspected=worker.grow_inspect(f.p)
            self.assertEqual(inspected["state"],"volume_extended")
            self.assertNotIn("grow_volume",inspected["verified_phases"])
            before=list(f.mutations)
            with self.assertRaises(RuntimeError):worker.grow_volume(f.p)
            self.assertEqual(f.mutations,before)

    def test_foreign_disk_boot_changes_and_config_drift_refuse_growth_before_mutation(self):
        for drift in ("disk","boot","config"):
            f=FakeGrowth()
            with self.subTest(drift=drift),f.patch():
                f.prepare()
                if drift=="disk":f.volume["guid"]="foreign"
                elif drift=="boot":f.boot_hash="changed"
                else:f.cfg["cores"]=8
                before=list(f.mutations)
                with self.assertRaises(RuntimeError):worker.grow_volume(f.p)
                self.assertEqual(f.mutations,before)
                self.assertEqual(worker.grow_inspect(f.p)["state"],"ambiguous")

    def test_partial_GPT_and_partial_filesystem_growth_remain_fenced(self):
        f=FakeGrowth()
        with f.patch():
            f.prepare();worker.grow_volume(f.p)
            with patch.object(worker,"declared_gpt_size",side_effect=RuntimeError("mixed GPT headers")):
                inspected=worker.grow_inspect(f.p)
                self.assertEqual(inspected["state"],"ambiguous")
                self.assertFalse(inspected["abort_safe"])
            worker.grow_partition(f.p)
            native=f.run
            def incomplete(*args,**kwargs):
                if args[0]=="resize2fs":return ""
                return native(*args,**kwargs)
            with patch.object(worker,"run",incomplete),self.assertRaises(RuntimeError):worker.grow_filesystem(f.p)
            self.assertNotIn("grow-filesystem",f.receipts)
            self.assertEqual(f.cfg["lock"],"migrate")
            before=list(f.mutations)
            with self.assertRaises(FileNotFoundError):worker.grow_unlock(f.p)
            self.assertEqual(f.mutations,before)

    def test_abort_only_before_actual_volume_extension(self):
        f=FakeGrowth()
        with f.patch():
            f.prepare();proof=worker.grow_abort(f.p)
            self.assertTrue(proof["aborted"])
            self.assertNotIn("lock",f.cfg);self.assertEqual(f.marker,"-")
            self.assertEqual(worker.grow_inspect(f.p)["state"],"aborted")
        f=FakeGrowth()
        with f.patch():
            f.prepare();worker.grow_volume(f.p);before=list(f.mutations)
            with self.assertRaises(RuntimeError):worker.grow_abort(f.p)
            self.assertEqual(f.mutations,before)

    def test_growth_root_geometry_never_shrinks_existing_root(self):
        f=FakeGrowth();baseline={"before":copy.deepcopy(f.layout)}
        sectors=worker.grow_geometry(f.p,baseline)
        self.assertGreater(sectors,f.layout["root_size"])
        self.assertLess((f.layout["root_start"]+sectors)*512,f.target*worker.MIB-33*512)
        with self.assertRaises(RuntimeError):worker.grow_geometry(dict(f.p,target_mib=f.old-1),baseline)

    def test_quick_inspection_never_hashes_partitions_or_promotes_candidates(self):
        f=FakeGrowth()
        with f.patch():
            f.prepare();worker.grow_volume(f.p);before=list(f.mutations)
            with patch.object(worker,"partition_hashes",side_effect=AssertionError("slow partition read")),\
                    patch.object(worker,"layout",side_effect=AssertionError("slow filesystem probe")),\
                    patch.object(worker,"declared_gpt_size",side_effect=AssertionError("raw disk read")):
                inspected=worker.grow_inspect(dict(f.p,quick=True))
            self.assertEqual(inspected["classification"],"full_verification_required")
            self.assertIn("grow_volume",inspected["candidate_phases"])
            self.assertEqual(inspected["verified_phases"],{})
            self.assertFalse(inspected["abort_safe"])
            self.assertFalse(inspected["bytes_verified"])
            self.assertEqual(f.mutations,before)

    def test_completed_filesystem_receipt_cannot_verify_a_reverted_attachment(self):
        f=FakeGrowth()
        with f.patch():
            f.partitioned();worker.grow_filesystem(f.p)
            f.cfg["scsi0"]=f.p["config"]["scsi0"]
            observed=worker.grow_inspect(f.p)
            self.assertFalse(observed["abort_safe"])
            self.assertNotIn("grow_filesystem",observed["verified_phases"])
            before=list(f.mutations)
            with self.assertRaises(RuntimeError):worker.grow_unlock(f.p)
            self.assertEqual(f.mutations,before)

    def test_valid_backup_GPT_before_zvol_end_is_checked_without_assuming_adjacency(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"disk.raw";span=4*1024**2;actual=8*1024**2;last=span//512-1
            entries=bytes(128*128)
            with path.open("wb") as stream:
                stream.truncate(actual)
                for own,alternate,lba in ((1,last,2),(last,1,last-32)):
                    header=bytearray(512);header[:8]=b"EFI PART"
                    struct.pack_into("<III",header,8,0x10000,92,0)
                    struct.pack_into("<4Q",header,24,own,alternate,34,last-33)
                    header[56:72]=bytes([3])*16
                    struct.pack_into("<QIII",header,72,lba,128,128,zlib.crc32(entries)&0xffffffff)
                    struct.pack_into("<I",header,16,zlib.crc32(header[:92])&0xffffffff)
                    stream.seek(own*512);stream.write(header);stream.seek(lba*512);stream.write(entries)
            self.assertEqual(worker.declared_gpt_size(path,actual),span)
            with self.assertRaises(RuntimeError):worker.declared_gpt_size(path,span-512)
            with path.open("r+b") as stream:stream.seek(span-512+16);stream.write(b"broken")
            with self.assertRaises(RuntimeError):worker.declared_gpt_size(path,actual)


if __name__=="__main__":unittest.main()
