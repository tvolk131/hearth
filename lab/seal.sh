#!/usr/bin/env bash
# Runs inside a fully built base as the last step before it is shut down and
# sealed (lab.py build). A sealed base may be cached or shared, so it must not
# hold the private lab SSH keys used to build it:
#   - every SSH host private key and root's own SSH key pair are shredded;
#   - cloud-init's cached copies of the build seed (which embed the host
#     private key) are shredded and cloud-init is disabled for good, so it
#     can never re-run against a Proxmox clone (see remote_migrate_spike.seed);
#   - a boot hook installs each run's disposable SSH identity from that run's
#     own seed CD before sshd starts.
# Proxmox regenerates root's key pair itself (pvecm updatecerts) at boot.
set -euo pipefail
test "$(hostname)" = pve-lab
test -f /var/lib/pve-dev-lab/ready
install -m 0600 /etc/pve-dev-lab-id /etc/pve-dev-lab-base-id

cat >/usr/local/sbin/pve-lab-ssh-identity <<'HOOK'
#!/bin/sh
# Installed by lab/seal.sh. Only a CD whose cloud-init instance-id is this
# base's nonce is trusted, never a nested guest's cloud-init volume.
set -eu
base=$(cat /etc/pve-dev-lab-base-id)
mount_point=$(mktemp -d)
trap 'umount "$mount_point" 2>/dev/null || true; rmdir "$mount_point"' EXIT
for _ in $(seq 60); do
    for device in /dev/sr*; do
        test -b "$device" || continue
        test "$(blkid -o value -s LABEL "$device" 2>/dev/null)" = cidata || continue
        mount -o ro "$device" "$mount_point"
        if python3 -c 'import json, sys; sys.exit(json.load(open(sys.argv[1]))["instance-id"] != sys.argv[2])' "$mount_point/meta-data" "$base"; then
            install -m 0600 "$mount_point/lab-host-key" /etc/ssh/ssh_host_ed25519_key
            install -m 0644 "$mount_point/lab-host-key.pub" /etc/ssh/ssh_host_ed25519_key.pub
            install -m 0644 "$mount_point/lab-authorized-keys" /etc/ssh/pve-lab/authorized_keys
            echo "pve-lab: installed this run's SSH identity from $device"
            exit 0
        fi
        umount "$mount_point"
    done
    sleep 1
done
echo "pve-lab: no seed CD for base $base; SSH identity not installed" >&2
exit 1
HOOK
chmod 0700 /usr/local/sbin/pve-lab-ssh-identity

cat >/etc/systemd/system/pve-lab-ssh-identity.service <<'UNIT'
[Unit]
Description=Install this disposable lab run's SSH identity from its seed CD
After=local-fs.target
Before=ssh.service ssh.socket

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/pve-lab-ssh-identity
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
UNIT

install -d -m 0755 /etc/ssh/pve-lab /etc/ssh/sshd_config.d
cat >/etc/ssh/sshd_config.d/10-pve-lab.conf <<'SSHD'
HostKey /etc/ssh/ssh_host_ed25519_key
AuthorizedKeysFile /etc/ssh/pve-lab/authorized_keys .ssh/authorized_keys
SSHD
sshd -t
systemctl daemon-reload
systemctl enable pve-lab-ssh-identity.service
touch /etc/cloud/cloud-init.disabled

# Overwrite in place before unlinking, so no key material survives in the
# freed blocks of the qcow2 image.
shopt -s nullglob
for secret in /etc/ssh/ssh_host_*_key /root/.ssh/id_rsa /root/.ssh/id_ecdsa /root/.ssh/id_ed25519 \
        /var/lib/cloud/instances/*/user-data.txt* /var/lib/cloud/instances/*/cloud-config.txt \
        /var/lib/cloud/instances/*/obj.pkl /var/lib/cloud/instances/*/combined-cloud-config.json; do
    # nullglob drops unmatched patterns, but literal paths stay in the list.
    test -e "$secret" || continue
    shred --zero --remove "$secret"
done
rm -f /etc/ssh/ssh_host_*_key.pub /root/.ssh/id_rsa.pub /root/.ssh/id_ecdsa.pub /root/.ssh/id_ed25519.pub
sync
fstrim --all || true
echo "Base sealed: no private SSH keys or cloud-init seed copies remain"
