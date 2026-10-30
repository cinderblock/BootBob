#!/bin/sh
# Build a small Debian disk image that a PC (or VM) runs from the network with BootBob:
# a FAT boot partition with the kernel and initrd, and an ext4 root filesystem the initrd
# mounts over NBD. Runs anywhere Docker does; nothing is installed on the host.
#
#   BOOTBOB=http://<bootbob>:8067 tools/make-pc-golden.sh <out.img> [size-GiB] [authorized_keys-file]
#
# With BOOTBOB set, bootbob-nbd-client and its initramfs hook are fetched from it and
# baked in, so machines' roots survive BootBob restarting: add
# NBDCLIENT=/usr/sbin/bootbob-nbd-client to the image's command line.
#
# Then upload it, compressed so its zeros don't cross the network (`zstd out.img`, then
# `curl -T out.img.zst -H 'Content-Encoding: zstd' http://<bootbob>:8067/api/disks/out.img`)
# and make a
# BootBob image with a `disk` section: source out.img, kernel vmlinuz, initrd initrd.img,
# command line `root=/dev/nbd0p2 nbdroot={server},{disk},nbd0 ip=dhcp
# NBDCLIENT=/usr/sbin/bootbob-nbd-client`, copy-on-write on.
#
# The image: Debian trixie amd64, linux-image-amd64, nbd-client (its initramfs hook
# brings NBD root support), openssh-server with the given authorized_keys for root,
# hostname bootbob-pc. The machine's hostname comes from DHCP if the network gives one.

set -eu
OUT=${1:?usage: make-pc-golden.sh <out.img> [size-GiB] [authorized_keys-file]}
SIZE_GIB=${2:-4}
KEYS=${3:-}
OUTDIR=$(cd "$(dirname "$OUT")" && pwd)
NAME=$(basename "$OUT")
[ -z "$KEYS" ] || [ -f "$KEYS" ] || { echo "no such keys file: $KEYS" >&2; exit 1; }
KEYMOUNT=""
[ -n "$KEYS" ] && KEYMOUNT="-v $(cd "$(dirname "$KEYS")" && pwd)/$(basename "$KEYS"):/keys:ro"
TOOLS=$(mktemp -d)
trap 'rm -rf "$TOOLS"' EXIT
if [ -n "${BOOTBOB:-}" ]; then
  curl -fsS -o "$TOOLS/bootbob-nbd-client" "$BOOTBOB/files/tools/bootbob-nbd-client-x86_64"
  curl -fsS -o "$TOOLS/bootbob-nbd-client.hook" "$BOOTBOB/files/tools/bootbob-nbd-client.hook"
else
  echo "warning: BOOTBOB unset, so no bootbob-nbd-client: the image's root won't survive BootBob restarting" >&2
fi

# mmdebstrap needs to mount /proc in the new root and run package scripts in a chroot,
# hence --privileged. The container is thrown away afterwards.
# shellcheck disable=SC2086
docker run --rm --privileged -v "$OUTDIR:/out" -v "$TOOLS:/tools:ro" $KEYMOUNT -e NAME="$NAME" -e SIZE_GIB="$SIZE_GIB" debian:trixie sh -euc '
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq mmdebstrap dosfstools e2fsprogs fdisk mtools >/dev/null

ROOT=/work/root
mkdir -p /work
mmdebstrap --variant=important --include=linux-image-amd64,initramfs-tools,nbd-client,openssh-server,systemd-sysv,udev,iproute2,ca-certificates \
  trixie "$ROOT" http://deb.debian.org/debian

echo bootbob-pc > "$ROOT/etc/hostname"
printf "127.0.1.1\tbootbob-pc\n" >> "$ROOT/etc/hosts"
# The root is /dev/nbd0p2, set up by the initrd. Nothing mounts the boot partition: the
# kernel and initrd in it are what this image was built with.
printf "/dev/nbd0p2\t/\text4\tdefaults,noatime\t0\t1\n" > "$ROOT/etc/fstab"
# Nothing in the image configures the network: the interface the initrd set up (with
# ip=dhcp) stays as it is, which matters, because the root filesystem is behind it.
if [ -f /keys ]; then
  install -d -m 0700 "$ROOT/root/.ssh"
  install -m 0600 /keys "$ROOT/root/.ssh/authorized_keys"
fi
if [ -f /tools/bootbob-nbd-client ]; then
  install -m 0755 /tools/bootbob-nbd-client "$ROOT/usr/sbin/bootbob-nbd-client"
  install -m 0755 /tools/bootbob-nbd-client.hook "$ROOT/etc/initramfs-tools/hooks/bootbob-nbd-client"
fi
# A serial console, so the VM can be watched without a screen.
mkdir -p "$ROOT/etc/systemd/system/getty.target.wants"
ln -sf /lib/systemd/system/serial-getty@.service "$ROOT/etc/systemd/system/getty.target.wants/serial-getty@ttyS0.service"
# Fresh SSH host keys and machine-id per machine would need a first-boot step; for a test
# image every copy shares them.
chroot "$ROOT" update-initramfs -u -k all >/dev/null

KVER=$(ls "$ROOT/boot" | sed -n "s/^vmlinuz-//p" | sort -V | tail -1)
BOOT_MB=256
TOTAL_MB=$((SIZE_GIB * 1024))
ROOT_MB=$((TOTAL_MB - BOOT_MB - 1))

# The boot partition: kernel and initrd under fixed names.
mkfs.vfat -F 32 -n BOOT -C /work/boot.fat $((BOOT_MB * 1024)) >/dev/null
mcopy -i /work/boot.fat "$ROOT/boot/vmlinuz-$KVER" ::vmlinuz
mcopy -i /work/boot.fat "$ROOT/boot/initrd.img-$KVER" ::initrd.img
# The root partition, filled straight from the tree (no mounting).
mkfs.ext4 -q -L root -d "$ROOT" /work/root.ext4 "${ROOT_MB}M"

# The disk: MBR, boot at 1 MiB, root after it.
IMG="/out/$NAME.part"
truncate -s "${TOTAL_MB}M" "$IMG"
printf "label: dos\nstart=2048, size=$((BOOT_MB * 2048)), type=c\nstart=$((2048 + BOOT_MB * 2048)), type=83\n" | sfdisk -q "$IMG"
dd if=/work/boot.fat of="$IMG" bs=1M seek=1 conv=notrunc,sparse status=none
dd if=/work/root.ext4 of="$IMG" bs=1M seek=$((1 + BOOT_MB)) conv=notrunc,sparse status=none
mv "$IMG" "/out/$NAME"
echo "built /out/$NAME: kernel $KVER, $SIZE_GIB GiB"
'
echo "Done: $OUT"
