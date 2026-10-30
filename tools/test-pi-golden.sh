#!/bin/sh
# Boot a make-pi-golden.sh image in an arm64 VM, over NBD, and check it from the inside.
# The Pi kernels and firmware can't run in QEMU's virt machine, so this uses the generic
# kernel TEST_KERNEL=1 builds; everything else is the real image: the initramfs's NBD
# root with bootbob-nbd-client, first boot, networking, and the root surviving the NBD
# server restarting. Only real Pi hardware can prove the firmware's network boot.
#
#   tools/test-pi-golden.sh <out.img>
#
# Needs qemu-system-aarch64, qemu-nbd, mtools and ssh. Works on a copy; out.img is left
# as it was.

set -eu
IMG=${1:?usage: test-pi-golden.sh <out.img>}
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
    -o ConnectTimeout=10 -o BatchMode=yes root@127.0.0.1 "$@"
}

i=0
until vm true 2>/dev/null; do
  kill -0 "$VM_PID" 2>/dev/null || fail "the VM stopped"
  i=$((i + 1))
  [ "$i" -lt 90 ] || fail "no SSH after 15 minutes"
  sleep 10
done
echo "== booted after about $((i * 10)) s; checking from inside"

check() {
  what=$1
  shift
  vm "$@" >"$WORK/out" 2>&1 || { cat "$WORK/out" >&2; fail "$what"; }
  echo "ok: $what: $(tr '\n' ' ' <"$WORK/out")"
}
check "root is the NBD disk, read-write" 'findmnt -no SOURCE,OPTIONS / | grep -E "^/dev/nbd0p2 +rw"'
check "boot partition is mounted" 'findmnt -no SOURCE /boot/firmware | grep -x /dev/nbd0p1'
check "the resident client is running" 'ps -eo args | grep "^@" | grep -i nbd'
check "first boot made SSH host keys" 'test -s /etc/ssh/ssh_host_ed25519_key.pub && ls /etc/ssh/ssh_host_ed25519_key.pub'
check "machine-id exists" 'test -s /etc/machine-id && cat /etc/machine-id'
check "no failed units" 'test "$(systemctl --failed --no-legend | wc -l)" = 0 || { systemctl --failed --no-pager; exit 1; }'
check "networkd manages the interface and kept the address" 'networkctl --no-pager | grep -E "e[a-z0-9]+ +ether +routable +configured" && ip -4 -br addr | grep 10.0.2'
check "DNS works" 'getent hosts deb.debian.org'
check "apt reaches both repositories" 'apt-get update 2>&1 | tail -2; apt-cache policy raspi-firmware | grep -q archive.raspberrypi.com'
check "the Pi kernels are in the boot partition" 'ls /boot/firmware/kernel8.img /boot/firmware/kernel_2712.img /boot/firmware/initramfs8'

echo "== restarting the NBD server under a writing machine"
vm 'nohup sh -c "i=0; while [ \$i -lt 60 ]; do echo \$i >> /var/tmp/writes; sync; i=\$((i + 1)); sleep 1; done" >/dev/null 2>&1 &'
sleep 5
kill "$NBD_PID"
wait "$NBD_PID" 2>/dev/null || true
sleep 3
serve
sleep 15
check "root survived the restart, still read-write" 'findmnt -no OPTIONS / | grep -q "^rw" && wc -l < /var/tmp/writes'
check "no I/O errors" '! dmesg | grep -E "I/O error|Remounting filesystem read-only|EXT4-fs error"'
echo "PASSED"
