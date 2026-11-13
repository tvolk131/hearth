#!/usr/bin/env bash
set -euo pipefail
exec >>/var/log/pve-lab-bootstrap.log 2>&1
export DEBIAN_FRONTEND=noninteractive
trap 'printf "bootstrap failed at line %s\n" "$LINENO" >/var/lib/pve-dev-lab/failed' ERR
mkdir -p /var/lib/pve-dev-lab
rm -f /var/lib/pve-dev-lab/failed
test "$(hostname)" = pve-lab
test -f /etc/pve-dev-lab-id
if ! grep -q '10.0.2.15 pve-lab.test pve-lab' /etc/hosts; then
    printf '\n10.0.2.15 pve-lab.test pve-lab\n' >>/etc/hosts
fi
if ! test -f /var/lib/pve-dev-lab/kernel-installed; then
    printf 'grub-pc grub-pc/install_devices multiselect /dev/vda\ngrub-pc grub-pc/install_devices_empty boolean false\n' | debconf-set-selections
    curl --fail --location --retry 3 'https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg' -o /usr/share/keyrings/proxmox-archive-keyring.gpg
    printf '136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45  /usr/share/keyrings/proxmox-archive-keyring.gpg\n' | sha256sum --check
    printf 'deb [signed-by=/usr/share/keyrings/proxmox-archive-keyring.gpg] http://download.proxmox.com/debian/pve trixie pve-no-subscription\n' >/etc/apt/sources.list.d/pve-install.list
    apt-get -o Acquire::Retries=3 update
    apt-get -o Acquire::Retries=3 install -y proxmox-default-kernel
    touch /var/lib/pve-dev-lab/kernel-installed
    systemctl reboot
    exit 0
fi
if ! uname -r | grep -q -- '-pve$'; then
    printf 'Proxmox kernel was installed but not booted\n' >&2
    exit 1
fi
if ! test -f /var/lib/pve-dev-lab/ready; then
    printf 'postfix postfix/mailname string pve-lab.test\npostfix postfix/main_mailer_type select Local only\n' | debconf-set-selections
    apt-get -o Acquire::Retries=3 update
    apt-get -o Acquire::Retries=3 install -y proxmox-ve postfix open-iscsi chrony zfsutils-linux
    systemctl enable --now pve-cluster pvedaemon pveproxy pvestatd
    pveversion --verbose >/var/lib/pve-dev-lab/package-versions.txt
    dpkg-query -W >/var/lib/pve-dev-lab/dpkg-versions.txt
    touch /var/lib/pve-dev-lab/ready
fi
