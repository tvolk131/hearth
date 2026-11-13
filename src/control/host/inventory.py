"""Bounded, read-only host topology. Never opens guest disks or SSH material."""
import datetime
import json
import pathlib
import subprocess
import sys


def run(*argv):
    return subprocess.run(argv, check=True, capture_output=True, text=True, timeout=30).stdout


node = sys.argv[1]
assert node and all(c.isalnum() or c in '-_.' for c in node)
status = json.loads(run('pvesh', 'get', '/nodes/' + node + '/status', '--output-format', 'json'))
block = json.loads(run('lsblk', '--json', '--bytes', '--output', 'NAME,PATH,TYPE,SIZE,MODEL,SERIAL,ROTA,FSTYPE,MOUNTPOINTS'))
physical_disks = [disk for disk in block.get('blockdevices', []) if disk['type'] == 'disk'
                  and not pathlib.Path('/sys/class/block', disk['name']).resolve().is_relative_to('/sys/devices/virtual')]
pool = run('zpool', 'list', '-Hp', '-o', 'size,alloc,free', 'rpool').strip().split('\t')
properties = ['name', 'type', 'guid', 'used', 'available', 'quota', 'reservation', 'refreservation', 'volsize',
              'usedbydataset', 'usedbysnapshots', 'usedbychildren', 'usedbyrefreservation', 'origin']
rows = run('zfs', 'list', '-Hp', '-r', '-t', 'filesystem,volume', '-o', ','.join(properties), 'rpool').splitlines()
assert len(rows) <= 2048
datasets = [dict(zip(properties, row.split('\t'), strict=True)) for row in rows]
vms = json.loads(run('pvesh', 'get', '/nodes/' + node + '/qemu', '--output-format', 'json'))
assert len(vms) <= 4096
result = {
    'schema': 1, 'node': node, 'captured_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'cpu_model': status.get('cpuinfo', {}).get('model', 'Unknown'),
    'logical_cpus': status.get('cpuinfo', {}).get('cpus', 0),
    'cpu_sockets': status.get('cpuinfo', {}).get('sockets', 0),
    'memory_total_bytes': status.get('memory', {}).get('total', 0),
    'memory_used_bytes': status.get('memory', {}).get('used', 0),
    'uptime_seconds': status.get('uptime', 0),
    'pve_version': status.get('pveversion', 'Unknown'),
    'block_devices': physical_disks,
    'pool_size_bytes': int(pool[0]), 'pool_allocated_bytes': int(pool[1]), 'pool_free_bytes': int(pool[2]),
    'zpool_topology': run('zpool', 'status', '-P', 'rpool'),
    'datasets': datasets,
    'vms': [{k: v.get(k) for k in ('vmid', 'name', 'status', 'mem', 'maxmem', 'cpus', 'uptime')} for v in vms],
}
encoded = json.dumps(result)
assert len(encoded) <= 160 * 1024
print(encoded)
