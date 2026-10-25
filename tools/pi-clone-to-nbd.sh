#!/bin/sh
# Clone the running Raspberry Pi onto a BootBob disk image, so it can boot from the
# network with no SD card: boot files over TFTP, root filesystem over NBD.
#
#   sudo pi-clone-to-nbd.sh <bootbob-host> <disk>.img [size-GiB]
#
# Run it on the Pi. Before it, in BootBob: give the Pi a Pi image whose boot files are
# <disk>.img, and assign the Pi to it (an NBD disk is only exported to the machine
# assigned it). The script creates the disk if it doesn't exist yet (POST /api/disks).
#
# What it does:
#   1. connects /dev/nbd0 to the disk and partitions it like a Pi OS card (FAT boot,
#      ext4 root), with a new disk id so it can't be mistaken for the SD card;
#   2. copies /boot/firmware and / into it (twice: the second pass catches up on what
#      changed during the first);
#   3. points the copy's fstab and cmdline.txt at the new partitions and the NBD root.
#
# Needs: nbd-client (whose initramfs hook puts NBD root support into the initramfs),
# rsync, dosfstools, curl. It never touches the SD card or the EEPROM.

set -eu

HOST=${1:?usage: pi-clone-to-nbd.sh <bootbob-host> <disk>.img [size-GiB]}
DISK=${2:?usage: pi-clone-to-nbd.sh <bootbob-host> <disk>.img [size-GiB]}
SIZE_GIB=${3:-32}
DEV=/dev/nbd0
MNT=/mnt/bootbob-clone

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
for t in nbd-client rsync mkfs.vfat mkfs.ext4 sfdisk curl blkid; do
	command -v "$t" >/dev/null || { echo "missing $t" >&2; exit 1; }
done
[ -e /usr/share/initramfs-tools/scripts/local-top/nbd ] || { echo "nbd-client's initramfs script is missing" >&2; exit 1; }

echo "== Creating $DISK (${SIZE_GIB} GiB) if it isn't there yet"
code=$(curl -s -o /tmp/bootbob-disk.out -w '%{http_code}' -X POST -H 'content-type: application/json' \
	-d "{\"name\":\"$DISK\",\"size\":$((SIZE_GIB * 1024 * 1024 * 1024))}" "http://$HOST:8067/api/disks")
case "$code" in
201) echo "created" ;;
*) grep -q "exists" /tmp/bootbob-disk.out && echo "already there" || { cat /tmp/bootbob-disk.out; exit 1; } ;;
esac

modprobe nbd
nbd-client -c "$DEV" >/dev/null 2>&1 && { echo "$DEV is in use" >&2; exit 1; }
nbd-client "$HOST" -N "$DISK" "$DEV" -b 512
cleanup() {
	umount "$MNT/boot/firmware" 2>/dev/null || true
	umount "$MNT" 2>/dev/null || true
	nbd-client -d "$DEV" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if blkid "${DEV}p2" >/dev/null 2>&1; then
	echo "== $DISK already has a filesystem: refreshing the copy, not repartitioning"
else
	echo "== Partitioning"
	ID=$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')
	sfdisk --quiet "$DEV" <<-EOF
		label: dos
		label-id: 0x$ID
		start=16384, size=1048576, type=c
		start=1064960, type=83
	EOF
	blockdev --rereadpt "$DEV"
	sleep 1
	mkfs.vfat -F 32 -n bootfs "${DEV}p1" >/dev/null
	mkfs.ext4 -q -L rootfs "${DEV}p2"
fi

PARTUUID_BOOT=$(blkid -s PARTUUID -o value "${DEV}p1")
PARTUUID_ROOT=$(blkid -s PARTUUID -o value "${DEV}p2")
mkdir -p "$MNT"
mount "${DEV}p2" "$MNT"
mkdir -p "$MNT/boot/firmware"
mount "${DEV}p1" "$MNT/boot/firmware"

for pass in 1 2; do
	echo "== Copying, pass $pass"
	# -x: only the root filesystem (mount points come across empty).
	rsync -aHAXx --numeric-ids --delete --exclude="$MNT" / "$MNT/"
	rsync -rt --delete /boot/firmware/ "$MNT/boot/firmware/"
done

echo "== Pointing the copy at its new home"
sed -i -E \
	-e "s#^PARTUUID=[0-9a-f-]+-01([[:space:]])#PARTUUID=$PARTUUID_BOOT\1#" \
	-e "s#^PARTUUID=[0-9a-f-]+-02([[:space:]])#PARTUUID=$PARTUUID_ROOT\1#" \
	"$MNT/etc/fstab"
# The root is the NBD disk; ip=dhcp brings the network up in the initramfs.
sed -i -E \
	-e 's#root=[^ ]+#root=/dev/nbd0p2#' \
	-e "s#\$# nbdroot=$HOST,$DISK,nbd0 ip=dhcp#" \
	"$MNT/boot/firmware/cmdline.txt"
grep -q nbdroot "$MNT/boot/firmware/cmdline.txt"

echo "== Done"
echo "fstab:"
grep -v '^#' "$MNT/etc/fstab"
echo "cmdline.txt:"
cat "$MNT/boot/firmware/cmdline.txt"
sync
