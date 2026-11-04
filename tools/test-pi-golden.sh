#!/bin/sh
# Boot a make-pi-golden.sh image in an arm64 VM, over NBD, and check it from the inside.
# The Pi kernels and firmware can't run in QEMU's virt machine, so this uses the generic
# kernel TEST_KERNEL=1 builds; everything else is the real image: the initramfs's NBD
# root with bootbob-nbd-client, first boot, the pi user and the base packages, networking,
# and the root surviving the NBD server restarting. Only real Pi hardware proves the
# firmware's network boot.
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

# The key logs in pi, the only user it was given to. as_pi runs a command as pi; vm runs it
# as root, through sudo without a password, as everything that needs root does.
as_pi() {
  ssh -q -i "$WORK/key" -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 -o BatchMode=yes pi@127.0.0.1 "$@"
}
vm() {
  # The command travels as base64, so no quoting of it can go wrong on the way.
  as_pi "sudo -n sh -c \"\$(echo $(printf %s "$*" | base64 | tr -d '\n') | base64 -d)\""
}

i=0
until as_pi true 2>/dev/null; do
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
check_pi() {
  what=$1
  shift
  as_pi "$@" >"$WORK/out" 2>&1 || { cat "$WORK/out" >&2; fail "$what"; }
  echo "ok: $what: $(tr '\n' ' ' <"$WORK/out")"
}
vm 'systemctl is-system-running --wait >/dev/null 2>&1' || true
check_pi "pi is the login user, with bash, no password, and sudo needing none" 'test "$(id -un)" = pi && test "$(getent passwd pi | cut -d: -f7)" = /bin/bash && sudo -n passwd -S pi | grep -E "^pi L" && sudo -n true && echo yes'
check_pi "pi has its groups" 'id -nG | tr " " "\n" | grep -cxE "adm|dialout|sudo|video|plugdev|input" | grep -x 6'
check_pi "a machine resolves its own name, so sudo does not complain" 'getent hosts "$(hostname)" && ! sudo -n true 2>&1 | grep -q "unable to resolve"'
check_pi "python3 makes a venv, and the compiler builds" 'python3 -m venv /tmp/v && /tmp/v/bin/python -c "print(1)" && printf "int main(void) { return 0; }\n" > /tmp/t.c && cc -o /tmp/t /tmp/t.c && /tmp/t && echo built'
check "the key went to pi, not root" 'test -s /home/pi/.ssh/authorized_keys && test ! -e /root/.ssh/authorized_keys && stat -c "%U %a" /home/pi/.ssh/authorized_keys'
check "the base packages are there" 'for p in wpasupplicant rsync curl git htop btop python3-venv build-essential avahi-daemon; do dpkg-query -W -f "\${db:Status-Status} \${Package}\n" "$p" | grep "^installed " || { echo "missing $p"; exit 1; }; done'
check "no cron: systemd timers do its job" '! dpkg-query -W -f "\${db:Status-Status}\n" cron cron-daemon-common 2>/dev/null | grep -q installed && ! command -v cron && test ! -e /etc/crontab && echo none'
check "avahi is running" 'systemctl is-active avahi-daemon'
check "root is the NBD disk, read-write" 'findmnt -no SOURCE,OPTIONS / | grep -E "^/dev/nbd0p2 +rw"'
check "boot partition is mounted" 'findmnt -no SOURCE /boot/firmware | grep -x /dev/nbd0p1'
check "the resident client is running" 'ps -eo args | grep "^@" | grep -i nbd'
check "first boot made SSH host keys" 'test -s /etc/ssh/ssh_host_ed25519_key.pub && ls /etc/ssh/ssh_host_ed25519_key.pub'
check "machine-id exists" 'test -s /etc/machine-id && cat /etc/machine-id'
check "no failed units" 'test "$(systemctl --failed --no-legend | wc -l)" = 0 || { systemctl --failed --no-pager; exit 1; }'
check "networkd manages the interface and kept the address" 'networkctl --no-pager | grep -E "e[a-z0-9]+ +ether +routable +configured" && ip -4 -br addr | grep 10.0.2'
check "DNS works" 'getent hosts deb.debian.org'
check "apt reaches both repositories" 'apt-get update 2>&1 | tail -2; apt-cache policy raspi-firmware | grep -q archive.raspberrypi.com'
check "EEPROM updates stay off: changing the EEPROM is a deliberate act" 'test "$(systemctl is-enabled rpi-eeprom-update)" = masked'
check "The rollout timer is on, and does nothing without bootbob.url" 'systemctl is-enabled bootbob-rollout.timer && /usr/sbin/bootbob-rollout'
check "Wi-Fi and Bluetooth firmware" 'ls /lib/firmware/brcm | grep -q brcmfmac43455-sdio && ls /lib/firmware/brcm | grep -q BCM4345C0'
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
check "writes kept going after the restart" 'a=$(wc -l < /var/tmp/writes); sleep 10; b=$(wc -l < /var/tmp/writes); echo "$a then $b"; test "$b" -gt "$a"'
vm 'dmesg | grep -i nbd | tail -8' >&2 || true
check "no I/O errors" '! dmesg | grep -E "I/O error|Remounting filesystem read-only|EXT4-fs error"'
echo "PASSED"
