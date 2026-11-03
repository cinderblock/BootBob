#!/bin/sh
# Boot a make-pios-golden.sh image in an arm64 VM, over NBD, and check it from the inside.
# The Pi kernels and firmware can't run in QEMU's virt machine, so this uses the generic
# kernel TEST_KERNEL=1 builds; everything else is the real image: the initramfs's NBD
# root with bootbob-nbd-client, Pi OS's first boot, NetworkManager, the pi user, and the root
# surviving the NBD server restarting. Only real Pi hardware proves the firmware's network boot.
#
#   tools/test-pios-golden.sh <out.img>
#
# Needs qemu-system-aarch64, qemu-nbd, mtools and ssh. Works on a copy; out.img is left
# as it was.

set -eu
IMG=${1:?usage: test-pios-golden.sh <out.img>}
WORK=$(mktemp -d)
NBD_PID=""
VM_PID=""
cleanup() {
  [ -z "$VM_PID" ] || kill "$VM_PID" 2>/dev/null || true
  [ -z "$NBD_PID" ] || kill "$NBD_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT
fail() {
  echo "FAILED: $1" >&2
  echo "--- serial log" >&2
  tail -80 "$WORK/serial.log" >&2 || true
  exit 1
}

cp --sparse=always "$IMG" "$WORK/disk.img"
ssh-keygen -q -t ed25519 -N "" -f "$WORK/key"
mcopy -i "$WORK/disk.img@@8M" "$WORK/key.pub" ::authorized_keys

serve() {
  qemu-nbd -f raw -x golden -b 127.0.0.1 -p 10809 -t --cache=writeback "$WORK/disk.img" &
  NBD_PID=$!
}
serve
sleep 1

ACCEL=tcg
if [ -w /dev/kvm ] && [ "$(uname -m)" = aarch64 ]; then ACCEL=kvm; fi
qemu-system-aarch64 -M virt -accel "$ACCEL" -cpu max -smp 4 -m 2048 -nographic -no-reboot \
  -kernel "$IMG.test-vmlinuz" -initrd "$IMG.test-initrd" \
  -append "console=ttyAMA0 root=/dev/nbd0p2 nbdroot=10.0.2.2,golden,nbd0 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client rootwait" \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n0 \
  </dev/null >"$WORK/serial.log" 2>&1 &
VM_PID=$!

vm() {
  ssh -q -i "$WORK/key" -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 -o BatchMode=yes pi@127.0.0.1 "$@"
}

i=0
until vm true 2>/dev/null; do
  kill -0 "$VM_PID" 2>/dev/null || fail "the VM stopped"
  i=$((i + 1))
  [ "$i" -lt 90 ] || fail "no SSH after 15 minutes"
  sleep 10
done
echo "== booted after about $((i * 10)) s; checking from inside"
# Pi OS's first boot (cloud-init) can still be running when SSH answers.
vm 'sudo cloud-init status --wait >/dev/null 2>&1; systemctl is-system-running --wait >/dev/null 2>&1' || true

check() {
  what=$1
  shift
  vm "$@" >"$WORK/out" 2>&1 || { cat "$WORK/out" >&2; fail "$what"; }
  echo "ok: $what: $(tr '\n' ' ' <"$WORK/out")"
}
check "it is Pi OS" 'grep -q "Raspberry Pi" /etc/apt/sources.list.d/raspi.sources && dpkg -s raspberrypi-sys-mods raspi-config network-manager >/dev/null && echo yes'
check "root is the NBD disk, read-write" 'findmnt -no SOURCE,OPTIONS / | grep -E "^/dev/nbd0p2 +rw"'
check "boot partition is mounted" 'findmnt -no SOURCE /boot/firmware | grep -x /dev/nbd0p1'
check "the root filesystem was grown" 'df -BG --output=size / | tail -1 | tr -d " G" | awk "{ exit !(\$1 > 20) }" && df -h --output=size / | tail -1'
check "the resident client is running" 'ps -eo args | grep "^@" | grep -i nbd'
check "pi logs in with the key, has bash and no password, and sudo needs none" 'test "$(getent passwd pi | cut -d: -f7)" = /bin/bash && sudo -n passwd -S pi | grep -E "^pi L" && sudo -n true'
check "pi has the Pi OS groups" 'id -nG | tr " " "\n" | grep -cxE "gpio|i2c|spi|dialout|video|sudo" | grep -x 6'
check "first boot made SSH host keys" 'test -s /etc/ssh/ssh_host_ed25519_key.pub && ls /etc/ssh/ssh_host_ed25519_key.pub'
check "machine-id exists" 'grep -qE "^[0-9a-f]{32}$" /etc/machine-id && cat /etc/machine-id'
check "no hostname built in" 'test ! -e /etc/hostname -o "$(cat /etc/hostname 2>/dev/null)" != raspberrypi && hostname'
check "no failed units" 'test "$(systemctl --failed --no-legend | wc -l)" = 0 || { systemctl --failed --no-pager; exit 1; }'
check "NetworkManager has the interface and kept the address" 'nmcli -t -f DEVICE,TYPE,STATE device | grep -E "^e[a-z0-9]+:ethernet:connected" && ip -4 -br addr | grep 10.0.2'
check "DNS works" 'getent hosts deb.debian.org'
check "apt reaches both repositories" 'sudo -n apt-get update 2>&1 | tail -2; apt-cache policy raspi-firmware | grep -q archive.raspberrypi.com'
check "git and btop, no cron" 'command -v git btop && ! dpkg -s cron >/dev/null 2>&1'
check "EEPROM updates are on, and skip boards without an EEPROM" 'systemctl is-enabled rpi-eeprom-update && systemctl show -p ConditionResult --value rpi-eeprom-update'
check "The rollout timer is on, and does nothing without bootbob.url" 'systemctl is-enabled bootbob-rollout.timer && sudo -n /usr/sbin/bootbob-rollout'
check "the Pi kernels are in the boot partition" 'ls /boot/firmware/kernel8.img /boot/firmware/kernel_2712.img /boot/firmware/initramfs8'
check "the boot partition's initramfs has bootbob-nbd-client" 'lsinitramfs /boot/firmware/initramfs8 | grep -q usr/sbin/bootbob-nbd-client && echo yes'

echo "== restarting the NBD server under a writing machine"
vm 'nohup sh -c "i=0; while [ \$i -lt 60 ]; do echo \$i >> /var/tmp/writes; sync; i=\$((i + 1)); sleep 1; done" >/dev/null 2>&1 &'
sleep 5
kill "$NBD_PID"
wait "$NBD_PID" 2>/dev/null || true
sleep 3
serve
sleep 15
check "root survived the restart, still read-write" 'findmnt -no OPTIONS / | grep -q "^rw" && wc -l < /var/tmp/writes'
check "writes kept going after the restart" 'a=$(wc -l < /var/tmp/writes); sleep 10; b=$(wc -l < /var/tmp/writes); echo "$a then $b"; test "$b" -gt "$a"'
vm 'sudo -n dmesg | grep -i nbd | tail -8' >&2 || true
check "no I/O errors" '! sudo -n dmesg | grep -E "I/O error|Remounting filesystem read-only|EXT4-fs error"'
echo "PASSED"
