#!/bin/sh
# Build a golden disk image for Raspberry Pis that run from the network with BootBob: a
# FAT boot partition (firmware, kernels, initramfs, config.txt) and an ext4 root
# filesystem the initramfs mounts over NBD. Built from packages, not cloned from a Pi, so
# it holds nothing of any machine's: every copy gets its own SSH host keys and machine-id
# on first boot, and its hostname from DHCP. Works on Pi 3, 4 and 5 (and Zero 2 W).
#
#   BOOTBOB=http://<bootbob>:8067 tools/make-pi-golden.sh <out.img> [size-GiB] [authorized_keys-file]
#
# Runs on any arm64 host with Docker, natively. On other hosts Docker has to be able to
# run arm64 binaries (binfmt_misc with qemu-aarch64 registered), which this script
# checks for and won't set up: that's a change to the host. TOOLS=<dir> instead of
# BOOTBOB takes bootbob-nbd-client-aarch64 and the hook from a directory. TEST_KERNEL=1
# also writes <out.img>.test-vmlinuz and .test-initrd: see tools/test-pi-golden.sh.
#
# Then upload it (`curl -T out.img http://<bootbob>:8067/api/disks/out.img`) and make a
# BootBob Pi image: boot files out.img, copy-on-write on, command line
#   console=ttyS0,115200 console=tty1 root=/dev/nbd0p2 nbdroot={server},out.img,nbd0 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client rootwait
# No SSH keys built in? Put one in the boot partition before uploading, and the first
# boot installs it for root: `mcopy -i out.img@@8M authorized_keys ::authorized_keys`.
#
# The image: Debian trixie arm64 and Raspberry Pi's repository (its kernels and firmware,
# updated by apt like any Pi OS: the boot partition is mounted at /boot/firmware, and
# TFTP reads boot files through the machine's overlay, so a machine's kernel update
# reaches its own next boot). Network by systemd-networkd, which keeps the address the
# initramfs set up: the root filesystem is behind it. No desktop, no default user.

set -eu
OUT=${1:?usage: make-pi-golden.sh <out.img> [size-GiB] [authorized_keys-file]}
SIZE_GIB=${2:-8}
KEYS=${3:-}
OUTDIR=$(cd "$(dirname "$OUT")" && pwd)
NAME=$(basename "$OUT")
[ -z "$KEYS" ] || [ -f "$KEYS" ] || { echo "no such keys file: $KEYS" >&2; exit 1; }
KEYMOUNT=""
[ -n "$KEYS" ] && KEYMOUNT="-v $(cd "$(dirname "$KEYS")" && pwd)/$(basename "$KEYS"):/keys:ro"

docker run --rm --platform linux/arm64 debian:trixie true 2>/dev/null || {
  echo "this Docker can't run arm64 binaries: build on an arm64 host, or register qemu-aarch64 with binfmt_misc" >&2
  exit 1
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
if [ -n "${TOOLS:-}" ]; then
  cp "$TOOLS/bootbob-nbd-client-aarch64" "$TOOLS/bootbob-nbd-client.hook" "$WORK/"
elif [ -n "${BOOTBOB:-}" ]; then
  curl -fsS -o "$WORK/bootbob-nbd-client-aarch64" "$BOOTBOB/files/tools/bootbob-nbd-client-aarch64"
  curl -fsS -o "$WORK/bootbob-nbd-client.hook" "$BOOTBOB/files/tools/bootbob-nbd-client.hook"
else
  echo "set BOOTBOB or TOOLS: without bootbob-nbd-client, roots don't survive BootBob restarting" >&2
  exit 1
fi

# mmdebstrap needs to mount /proc in the new root and run package scripts in a chroot,
# hence --privileged. The container is thrown away afterwards.
# shellcheck disable=SC2086
docker run --rm --privileged --platform linux/arm64 -v "$OUTDIR:/out" -v "$WORK:/tools:ro" $KEYMOUNT \
  -e NAME="$NAME" -e SIZE_GIB="$SIZE_GIB" -e TEST_KERNEL="${TEST_KERNEL:-}" debian:trixie sh -euc '
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq mmdebstrap dosfstools e2fsprogs fdisk mtools curl gnupg >/dev/null

mkdir -p /work
ROOT=/work/root
# The Raspberry Pi archive key, from its keyring package and checked against its
# well-known fingerprint. Not the copy on their website: that one carries only a SHA-1
# binding signature, which apt on trixie refuses. mmdebstrap runs apt from out here, then
# the image runs it from in there, so signed-by has to name a file in both.
PIREPO=http://archive.raspberrypi.com/debian
PIKEY=/usr/share/keyrings/raspberrypi-archive-keyring.pgp
curl -fsS "$PIREPO/dists/trixie/main/binary-arm64/Packages.gz" | gunzip > /work/Packages
DEB=$(sed -n "/^Package: raspberrypi-archive-keyring\$/,/^\$/s/^Filename: //p" /work/Packages)
curl -fsS -o /work/keyring.deb "$PIREPO/$DEB"
dpkg-deb -x /work/keyring.deb /work/keyring
mkdir -p "$(dirname "$PIKEY")"
cp "/work/keyring$PIKEY" "$PIKEY"
gpg --show-keys --with-colons "$PIKEY" | grep -q "^fpr:*CF8A1AF502A2AA2D763BAE7E82B129927FA3303E:" || {
  echo "the Raspberry Pi archive key has an unexpected fingerprint" >&2
  exit 1
}

mmdebstrap --architectures=arm64 --variant=important \
  --setup-hook="mkdir -p \"\$1\$(dirname $PIKEY)\" && cp $PIKEY \"\$1$PIKEY\"" \
  --include=linux-image-rpi-v8,linux-image-rpi-2712,raspi-firmware,raspberrypi-archive-keyring,rpi-eeprom,initramfs-tools,nbd-client,openssh-server,systemd-sysv,systemd-resolved,systemd-timesyncd,udev,dbus,iproute2,ca-certificates \
  trixie "$ROOT" \
  "deb http://deb.debian.org/debian trixie main contrib non-free non-free-firmware" \
  "deb http://deb.debian.org/debian-security trixie-security main contrib non-free non-free-firmware" \
  "deb [signed-by=$PIKEY] $PIREPO trixie main"

# Each machine makes its own on first boot. No /etc/hostname: DHCP names the machine.
rm -f "$ROOT"/etc/ssh/ssh_host_* "$ROOT/etc/hostname" "$ROOT/var/lib/dbus/machine-id"
: > "$ROOT/etc/machine-id"

# The initramfs set up /dev/nbd0 before anything here runs.
printf "/dev/nbd0p2\t/\text4\tdefaults,noatime\t0\t1\n/dev/nbd0p1\t/boot/firmware\tvfat\tdefaults\t0\t2\n" > "$ROOT/etc/fstab"
echo nbd >> "$ROOT/etc/initramfs-tools/modules"

cat > "$ROOT/etc/systemd/network/10-wired.network" <<EOF
# KeepConfiguration: the root filesystem is behind this interface, so its address must
# never drop, even if the DHCP server goes quiet. The initramfs brought it up.
[Match]
Name=e*

[Network]
DHCP=yes
KeepConfiguration=yes
EOF
ln -sf ../run/systemd/resolve/stub-resolv.conf "$ROOT/etc/resolv.conf"

cat > "$ROOT/usr/sbin/bootbob-first-boot" <<EOF
#!/bin/sh
# Makes what the golden image deliberately lacks. Runs once on each machine.
set -eu
ssh-keygen -A
if [ -f /boot/firmware/authorized_keys ]; then
  install -d -m 0700 /root/.ssh
  install -m 0600 /boot/firmware/authorized_keys /root/.ssh/authorized_keys
fi
EOF
chmod 755 "$ROOT/usr/sbin/bootbob-first-boot"
cat > "$ROOT/etc/systemd/system/bootbob-first-boot.service" <<EOF
[Unit]
Description=First boot of a BootBob golden image
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key
RequiresMountsFor=/boot/firmware
Before=ssh.service

[Service]
Type=oneshot
ExecStart=/usr/sbin/bootbob-first-boot

[Install]
WantedBy=multi-user.target
EOF
chroot "$ROOT" systemctl enable systemd-networkd systemd-resolved bootbob-first-boot >/dev/null

install -m 0755 /tools/bootbob-nbd-client-aarch64 "$ROOT/usr/sbin/bootbob-nbd-client"
install -m 0755 /tools/bootbob-nbd-client.hook "$ROOT/etc/initramfs-tools/hooks/bootbob-nbd-client"
chroot "$ROOT" update-initramfs -u -k all >/dev/null

FW="$ROOT/boot/firmware"
cat > "$FW/config.txt" <<EOF
# The firmware finds kernel8.img (kernel_2712.img on a Pi 5) and, with auto_initramfs,
# the matching initramfs8 (initramfs_2712).
arm_64bit=1
auto_initramfs=1
enable_uart=1
EOF
# Used only when a BootBob image gives no command line of its own, which it must: this
# one cannot name the disk.
echo "console=ttyS0,115200 console=tty1 root=/dev/nbd0p2 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client rootwait" > "$FW/cmdline.txt"
[ ! -f /keys ] || install -m 0600 /keys "$FW/authorized_keys"
for f in start4.elf fixup4.dat kernel8.img initramfs8 kernel_2712.img initramfs_2712 bcm2711-rpi-4-b.dtb; do
  [ -f "$FW/$f" ] || { echo "the boot partition has no $f:" >&2; ls "$FW" >&2; exit 1; }
done

BOOT_MB=512
TOTAL_MB=$((SIZE_GIB * 1024))
ROOT_MB=$((TOTAL_MB - BOOT_MB - 8))

# Partitions where tools/pi-clone-to-nbd.sh puts a Pi disk: 8 MiB in, 512 MiB boot.
mkfs.vfat -F 32 -n bootfs -C /work/boot.fat $((BOOT_MB * 1024)) >/dev/null
mcopy -s -i /work/boot.fat "$FW"/* ::
if [ -n "$TEST_KERNEL" ]; then cp -a "$ROOT" /work/testroot; fi
rm -rf "${FW:?}"/*
# The root partition, filled straight from the tree (no mounting).
mkfs.ext4 -q -L rootfs -d "$ROOT" /work/root.ext4 "${ROOT_MB}M"

IMG="/out/$NAME.part"
truncate -s "${TOTAL_MB}M" "$IMG"
printf "label: dos\nstart=16384, size=$((BOOT_MB * 2048)), type=c\nstart=$((16384 + BOOT_MB * 2048)), type=83\n" | sfdisk -q "$IMG"
dd if=/work/boot.fat of="$IMG" bs=1M seek=8 conv=notrunc,sparse status=none
dd if=/work/root.ext4 of="$IMG" bs=1M seek=$((8 + BOOT_MB)) conv=notrunc,sparse status=none

# For tools/test-pi-golden.sh. The Pi kernels only run on Pis, so a VM boots this image
# with the generic Debian kernel, installed here in a copy of the root: the image itself
# never holds it.
if [ -n "$TEST_KERNEL" ]; then
  T=/work/testroot
  rm -f "$T/etc/resolv.conf"
  cp /etc/resolv.conf "$T/etc/resolv.conf"
  printf "virtio_pci\nvirtio_net\n" >> "$T/etc/initramfs-tools/modules"
  chroot "$T" sh -c "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq linux-image-arm64" >/dev/null
  KVER=$(ls "$T/boot" | sed -n "s/^vmlinuz-\(.*-arm64\)$/\1/p" | sort -V | tail -1)
  cp "$T/boot/vmlinuz-$KVER" "/out/$NAME.test-vmlinuz"
  cp "$T/boot/initrd.img-$KVER" "/out/$NAME.test-initrd"
fi

mv "$IMG" "/out/$NAME"
echo "built /out/$NAME: $(ls "$ROOT/lib/modules" | tr "\n" " "), $SIZE_GIB GiB"
'
echo "Done: $OUT"
