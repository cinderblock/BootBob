#!/bin/bash
# TRIM against a real kernel: a VM whose root filesystem is on BootBob's NBD server, through
# the bootbob-nbd-client in its initramfs, once as a plain disk and once through a
# copy-on-write overlay. It proves that discard reaches the server, that fstrim gives the
# space back on the server's disk, and that a filesystem under load stays clean through
# server restarts, kills, and TRIMs slow enough that the kernel gives up on them and comes
# back while they're still running.
#
#   docker run --rm --device /dev/kvm -v "$PWD:/src:ro" -v <dir with the image>:/in:ro \
#     debian:trixie /src/tools/test-trim.sh /in/golden.img /src/backend/target/debug/examples/serve_dir
#
# Run it in a throwaway container like that: it installs packages and needs nothing else
# from the host. The VM's network is QEMU's user networking, so nothing touches the LAN.
# <golden.img> comes from tools/make-pc-golden.sh with BOOTBOB set, and is only read.
# Build the server first with `cargo build --example serve_dir -p bootbob-nbd`.
#
# EXPORTS picks the disk types ("plain.img base.cow" by default). Exits non-zero on any
# failure; the last lines say which.

set -eu
GOLDEN=${1:?usage: test-trim.sh <golden.img> <serve_dir binary>}
SERVER=${2:?usage: test-trim.sh <golden.img> <serve_dir binary>}
EXPORTS=${EXPORTS:-plain.img base.cow}
export DEBIAN_FRONTEND=noninteractive

apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends qemu-system-x86 qemu-utils mtools e2fsprogs fdisk openssh-client >/dev/null

WORK=$(mktemp -d /var/tmp/test-trim.XXXXXX)
DISKS=$WORK/disks
ADDR=127.0.0.1:10809
mkdir -p "$DISKS"
SERVER_PID=
QEMU_PID=

say() { printf '\n=== %s\n' "$*"; }
fail() {
	echo "FAILED: $*" >&2
	echo "--- server log" >&2
	tail -40 "$WORK/server.log" >&2 || true
	echo "--- serial log" >&2
	tail -40 "$WORK"/serial-*.log >&2 || true
	exit 1
}
cleanup() {
	[ -z "$QEMU_PID" ] || kill -9 "$QEMU_PID" 2>/dev/null || true
	[ -z "$SERVER_PID" ] || kill -9 "$SERVER_PID" 2>/dev/null || true
	rm -rf "$WORK"
}
trap cleanup EXIT

# Part 2 of the image starts here; part 1 holds the kernel and initrd.
sectors() { sfdisk -d "$1" | sed -n "s/^[^:]*$2 : start= *\([0-9]*\), size= *\([0-9]*\).*/\1 \2/p"; }
read -r BOOT_START _ <<<"$(sectors "$GOLDEN" 1)"
read -r ROOT_START ROOT_SECTORS <<<"$(sectors "$GOLDEN" 2)"
mcopy -i "$GOLDEN@@$((BOOT_START * 512))" ::vmlinuz ::initrd.img "$WORK/"

say "A throwaway disk with a throwaway key"
ssh-keygen -q -t ed25519 -N '' -f "$WORK/key"
cp --sparse=always "$GOLDEN" "$DISKS/plain.img"
dd if="$GOLDEN" of="$WORK/root.ext4" bs=512 skip="$ROOT_START" count="$ROOT_SECTORS" conv=sparse status=none
debugfs -w "$WORK/root.ext4" -f - >/dev/null 2>&1 <<EOF
mkdir /root/.ssh
rm /root/.ssh/authorized_keys
write $WORK/key.pub /root/.ssh/authorized_keys
sif /root/.ssh/authorized_keys mode 0100600
EOF
dd if="$WORK/root.ext4" of="$DISKS/plain.img" bs=512 seek="$ROOT_START" conv=notrunc,sparse status=none
rm "$WORK/root.ext4"
cp --sparse=always "$DISKS/plain.img" "$DISKS/base.img"
base_sum=$(sha256sum "$DISKS/base.img" | cut -d' ' -f1)

start_server() {
	"$SERVER" "$DISKS" "$ADDR" >>"$WORK/server.log" 2>&1 &
	SERVER_PID=$!
	for _ in $(seq 50); do
		grep -q "serving" "$WORK/server.log" && (: </dev/tcp/127.0.0.1/10809) 2>/dev/null && return
		sleep 0.1
	done
	fail "the server didn't start"
}

guest() { ssh -q -i "$WORK/key" -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 root@127.0.0.1 "$@"; }

wait_guest() {
	for _ in $(seq 150); do
		kill -0 "$QEMU_PID" 2>/dev/null || fail "the VM died"
		guest true 2>/dev/null && return
		sleep 2
	done
	fail "the VM never came up"
}

boot() {
	local accel=tcg
	[ -w /dev/kvm ] && accel=kvm
	qemu-system-x86_64 -accel "$accel" -m 1024 -smp 2 -display none -no-user-config -nodefaults \
		-kernel "$WORK/vmlinuz" -initrd "$WORK/initrd.img" \
		-append "console=ttyS0 root=/dev/nbd0p2 nbdroot=10.0.2.2,$1,nbd0 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client rootwait" \
		-netdev user,id=n,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n \
		-serial "file:$WORK/serial-$1.log" -monitor none &
	QEMU_PID=$!
	wait_guest
}

# Bytes the disk takes on the server, its overlay's for an overlay.
used() {
	case $1 in
	*.img) echo $(($(stat -c %b "$DISKS/$1") * 512)) ;;
	*.cow) echo $(($(stat -c %b "$DISKS/overlays/${1%.cow}.ovl") * 512)) ;;
	esac
}
mib() { echo $(($1 >> 20)); }

power_off() {
	guest 'systemctl poweroff' 2>/dev/null || true
	for _ in $(seq 60); do
		kill -0 "$QEMU_PID" 2>/dev/null || { QEMU_PID=; return; }
		sleep 1
	done
	fail "the VM didn't power off"
}

# Reads the whole disk through the server, as a third party (qemu's NBD client), and checks
# the filesystem on it.
fsck_export() {
	rm -f "$WORK/whole.raw" "$WORK/root.ext4"
	qemu-img convert -f raw -O raw "nbd://$ADDR/$1" "$WORK/whole.raw"
	dd if="$WORK/whole.raw" of="$WORK/root.ext4" bs=512 skip="$ROOT_START" count="$ROOT_SECTORS" conv=sparse status=none
	rm "$WORK/whole.raw"
	e2fsck -fn "$WORK/root.ext4" || fail "e2fsck found problems on $1"
	rm "$WORK/root.ext4"
}

no_io_errors() {
	if grep -E 'I/O error|EXT4-fs error|Remounting filesystem read-only|Aborting journal' "$WORK/serial-$1.log"; then
		fail "$1: the kernel reported errors"
	fi
}

scenario() {
	local export=$1
	say "$export: boot with the root on it"
	: >"$WORK/serial-$export.log"
	boot "$export"
	guest 'uname -r; pgrep -a -f @bootbob-nbd'
	local max gran zero
	max=$(guest cat /sys/block/nbd0/queue/discard_max_bytes)
	gran=$(guest cat /sys/block/nbd0/queue/discard_granularity)
	zero=$(guest cat /sys/block/nbd0/queue/write_zeroes_max_bytes)
	echo "discard_max_bytes=$max discard_granularity=$gran write_zeroes_max_bytes=$zero"
	[ "$max" -gt 0 ] || fail "$export: discard is off in the kernel"

	say "$export: fstrim gives space back"
	guest 'fstrim -v /'
	guest 'dd if=/dev/urandom of=/root/keep bs=1M count=50 status=none; sha256sum /root/keep >/root/keep.sum; sync'
	local start full after
	start=$(used "$export")
	guest 'dd if=/dev/urandom of=/root/big bs=1M count=400 status=none; sync'
	full=$(used "$export")
	guest 'rm /root/big; sync; fstrim -v /; sync'
	after=$(used "$export")
	echo "on the server: $(mib "$start") MiB, $(mib "$full") MiB with a 400 MiB file, $(mib "$after") MiB after deleting it and fstrim"
	[ "$((full - start))" -ge $((350 << 20)) ] || fail "$export: the file didn't take space"
	[ "$after" -le $((start + (40 << 20))) ] || fail "$export: fstrim gave back too little"
	guest 'echo 3 >/proc/sys/vm/drop_caches; sha256sum -c /root/keep.sum'

	say "$export: it survives a reboot"
	guest 'systemctl reboot' 2>/dev/null || true
	sleep 5
	wait_guest
	guest 'sha256sum -c /root/keep.sum; fstrim -v /'
	[ "$(guest cat /sys/block/nbd0/queue/discard_max_bytes)" -gt 0 ] || fail "$export: discard is off after a reboot"

	say "$export: writing, deleting and trimming while the server restarts"
	guest 'cat >/root/churn.sh' <<'EOF'
mkdir -p /root/churn
cd /root/churn
n=0
while [ ! -e /root/stop ]; do
	n=$((n + 1))
	dd if=/dev/urandom of=f$n bs=1M count=16 status=none conv=fsync
	sha256sum f$n >f$n.sum
	# Half go right away, and the rest later: freed blocks get reused while being trimmed.
	[ $((n % 2)) = 0 ] && rm -f f$((n - 1)) f$((n - 1)).sum
	rm -f f$((n - 40)) f$((n - 40)).sum
	sync
	fstrim / || echo "fstrim failed at $n" >>/root/churn.err
done
echo "$n rounds" >/root/churn.done
EOF
	guest 'nohup sh /root/churn.sh >/root/churn.log 2>&1 </dev/null &'
	sleep 15
	echo "SIGTERM"
	kill -TERM "$SERVER_PID"
	wait "$SERVER_PID" || true
	start_server
	sleep 15
	for i in 1 2; do
		echo "SIGKILL $i"
		kill -9 "$SERVER_PID"
		wait "$SERVER_PID" 2>/dev/null || true
		sleep 1
		start_server
		sleep 40 # a request in flight is sent again only when it times out, after 30 s
	done
	say "$export: TRIMs that outlast the kernel's patience"
	kill -TERM "$SERVER_PID"
	wait "$SERVER_PID" || true
	TRIM_STALL=7:40 start_server
	sleep 100
	grep -c 'stalling TRIM' "$WORK/server.log" || fail "$export: no TRIM ever stalled"
	kill -TERM "$SERVER_PID"
	wait "$SERVER_PID" || true
	start_server
	guest 'touch /root/stop'
	for _ in $(seq 120); do
		guest 'test -e /root/churn.done' && break
		sleep 2
	done
	guest 'cat /root/churn.done; ls /root/churn/*.sum | wc -l; test ! -e /root/churn.err || cat /root/churn.err'
	say "$export: every file still reads back"
	guest 'sync; echo 3 >/proc/sys/vm/drop_caches; cd /root/churn; for s in *.sum; do sha256sum -c --quiet "$s" || echo "BAD $s"; done | tee /root/churn.bad; test ! -s /root/churn.bad; sha256sum -c /root/keep.sum' ||
		fail "$export: data was lost"
	guest 'findmnt -no OPTIONS / | grep -q "^rw"' || fail "$export: the root isn't read-write"
	guest 'dmesg' | grep -E 'nbd|EXT4' | tail -30
	guest 'dmesg' >"$WORK/dmesg-$export.log"
	cat "$WORK/dmesg-$export.log" >>"$WORK/serial-$export.log"
	no_io_errors "$export"

	say "$export: power off, e2fsck over NBD"
	power_off
	fsck_export "$export"
	echo "$export: PASS"
}

start_server
for e in $EXPORTS; do
	scenario "$e"
done

say "The base under the overlay"
[ "$(sha256sum "$DISKS/base.img" | cut -d' ' -f1)" = "$base_sum" ] || fail "the base image changed"
echo "unchanged"
echo
echo "ALL PASSED"
