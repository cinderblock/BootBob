#!/bin/sh
# Build a golden disk image for Raspberry Pis that run from the network with BootBob, from
# Raspberry Pi OS Lite: the official 64-bit image, checked against Raspberry Pi's published
# checksum, with as little changed as a network root needs. For machines that want Pi OS
# itself (raspi-config, NetworkManager, the Pi's udev rules and groups, cloud-init). The
# leaner image built from packages is tools/make-pi-golden.sh.
#
#   BOOTBOB=http://<bootbob>:8067 tools/make-pios-golden.sh <out.img> [size-GiB] [authorized_keys-file]
#
# Runs on any arm64 host with Docker, natively, like make-pi-golden.sh (same TOOLS= and
# TEST_KERNEL=1 options; the test is tools/test-pios-golden.sh). PIOS_URL and PIOS_SHA256
# pick an image other than the latest Pi OS Lite.
#
# What changes from the official image:
#   - its root filesystem is grown to size-GiB (default 32), and boots from /dev/nbd0 with
#     bootbob-nbd-client in the initramfs, so the root survives BootBob restarting;
#   - packages are upgraded, and git, btop and libnss-myhostname added; cron is removed,
#     since systemd's timers do its job;
#   - the user is pi, as Pi OS always had: SSH keys only (no password), sudo without one,
#     with the Pi OS groups. The first boot installs /boot/firmware/authorized_keys for pi;
#   - SSH is on, with host keys made on each machine's first boot (Pi OS does that already);
#   - no hostname built in, so DHCP names each machine;
#   - the bootbob-rollout timer is there, and does nothing unless the command line names
#     BootBob; EEPROM updates stay on, as in Pi OS, on boards that have an EEPROM;
#   - enable_uart=1 for a serial console.
# Pi OS's machine-id is left "uninitialized", so each machine makes its own.
#
# Then upload it (`zstd out.img`, then `curl -T out.img.zst -H 'Content-Encoding: zstd'
# http://<bootbob>:8067/api/disks/out.img`) and make a BootBob Pi image: boot files out.img,
# copy-on-write on, command line
#   console=serial0,115200 console=tty1 root=/dev/nbd0p2 nbdroot={server},{disk},nbd0 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client bootbob.url={bootbob} rootwait
# Name the file for what it is and its date (raspios-trixie-arm64-20261108.img): an overlay
# is tied to one exact disk. No SSH key built in? Add one to the boot partition before
# uploading: `mcopy -i out.img@@8M authorized_keys ::authorized_keys`. Hardware settings
# (I2C, 1-Wire, ...) go in a machine's own config.txt, which lives in its overlay.

set -eu

if [ "${1:-}" = --inside ]; then
  # In a throwaway privileged arm64 container. /out, /tools, /machine and /keys are mounted.
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates jq xz-utils fdisk \
    e2fsprogs mtools util-linux >/dev/null

  if [ -z "${PIOS_URL:-}" ]; then
    # The list Raspberry Pi Imager reads: the latest Lite (64-bit), and its checksum.
    curl -fsS https://downloads.raspberrypi.com/os_list_imagingutility_v4.json > /work/os_list.json
    jq -r '[.. | objects | select(.name? == "Raspberry Pi OS Lite (64-bit)")][0] | .url, .extract_sha256' \
      /work/os_list.json > /work/pick
    PIOS_URL=$(sed -n 1p /work/pick)
    PIOS_SHA256=$(sed -n 2p /work/pick)
  fi
  case "$PIOS_URL" in https://downloads.raspberrypi.com/*.img.xz) ;; *)
    echo "unexpected Pi OS URL: $PIOS_URL" >&2; exit 1 ;;
  esac
  echo "Pi OS: $PIOS_URL"
  IMG="/out/$NAME.part"
  curl -fsS "$PIOS_URL" | xz -dc > "$IMG"
  echo "$PIOS_SHA256  $IMG" | sha256sum -c --quiet || { echo "Pi OS checksum mismatch" >&2; exit 1; }

  # Pi OS cards: 8 MiB in, FAT boot, then ext4 root. Grow the file, the partition, then ext4.
  sfdisk -d "$IMG" > /work/table
  BOOT_START=$(sed -n "s/^[^ ]*1 : start= *\([0-9]*\),.*/\1/p" /work/table)
  BOOT_SIZE=$(sed -n "s/^[^ ]*1 : start= *[0-9]*, size= *\([0-9]*\),.*/\1/p" /work/table)
  ROOT_START=$(sed -n "s/^[^ ]*2 : start= *\([0-9]*\),.*/\1/p" /work/table)
  [ "$BOOT_START" = 16384 ] || { echo "unexpected partition table:" >&2; cat /work/table >&2; exit 1; }
  truncate -s "${SIZE_GIB}G" "$IMG"
  echo ", +" | sfdisk -q --no-reread -N 2 "$IMG"
  ROOT_SIZE=$(sfdisk -d "$IMG" | sed -n "s/^[^ ]*2 : start= *[0-9]*, size= *\([0-9]*\),.*/\1/p")
  ROOTDEV=$(losetup -f --show -o $((ROOT_START * 512)) --sizelimit $((ROOT_SIZE * 512)) "$IMG")
  BOOTDEV=$(losetup -f --show -o $((BOOT_START * 512)) --sizelimit $((BOOT_SIZE * 512)) "$IMG")
  R=/work/root
  cleanup() {
    for m in dev/pts dev sys proc etc/resolv.conf boot/firmware ""; do
      mountpoint -q "$R/$m" 2>/dev/null && umount "$R/$m"
    done
    losetup -d "$ROOTDEV" "$BOOTDEV" 2>/dev/null || true
  }
  trap cleanup EXIT
  e2fsck -fy "$ROOTDEV" >/dev/null || [ $? -le 1 ]
  resize2fs -p "$ROOTDEV" >/dev/null 2>&1
  mkdir -p "$R"
  mount "$ROOTDEV" "$R"
  mount "$BOOTDEV" "$R/boot/firmware"
  mount -t proc proc "$R/proc"
  mount -t sysfs sys "$R/sys"
  mount --bind /dev "$R/dev"
  mount --bind /dev/pts "$R/dev/pts"
  mount --bind /etc/resolv.conf "$R/etc/resolv.conf"
  ch() { chroot "$R" env DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 "$@"; }

  ch apt-get update -qq
  ch apt-get full-upgrade -y -qq -o Dpkg::Options::=--force-confold >/dev/null
  # libnss-myhostname: a machine always resolves its own name, even one DHCP gave it.
  ch apt-get install -y -qq nbd-client git btop libnss-myhostname >/dev/null
  # Only cron itself may go: anything else apt would take with it fails the build.
  ch apt-get purge -s cron cron-daemon-common | sed -n "s/^Purg \([^ ]*\).*/\1/p" | sort > /work/purge
  printf "cron\ncron-daemon-common\n" | sort | cmp -s - /work/purge || {
    echo "removing cron would also remove:" >&2; cat /work/purge >&2; exit 1
  }
  ch apt-get purge -y -qq cron cron-daemon-common >/dev/null
  ch apt-get autoremove -y -qq --purge >/dev/null
  ch apt-get clean

  # The initramfs connects /dev/nbd0 before anything here runs. nofail on the boot
  # partition: without it a machine still comes up, to be fixed.
  printf "proc\t/proc\tproc\tdefaults\t0\t0\n/dev/nbd0p1\t/boot/firmware\tvfat\tdefaults,nofail\t0\t2\n/dev/nbd0p2\t/\text4\tdefaults,noatime\t0\t1\n" > "$R/etc/fstab"
  grep -qx nbd "$R/etc/initramfs-tools/modules" || echo nbd >> "$R/etc/initramfs-tools/modules"
  # Pi OS builds its initramfs with MODULES=dep: only what the machine building it needs.
  # Built here, that would be this container's host, not a Pi, so take them all.
  printf "# One image for every Pi it boots: dep would fit the initramfs to the build host.\nMODULES=most\n" \
    > "$R/etc/initramfs-tools/conf.d/bootbob-modules"
  install -m 0755 /tools/bootbob-nbd-client-aarch64 "$R/usr/sbin/bootbob-nbd-client"
  install -m 0755 /tools/bootbob-nbd-client.hook "$R/etc/initramfs-tools/hooks/bootbob-nbd-client"

  # pi: Pi OS made it, with its groups, but with no login until its first-boot dialog runs.
  ch usermod -s /bin/bash pi
  ch passwd -l pi >/dev/null
  printf "pi ALL=(ALL) NOPASSWD: ALL\n" > "$R/etc/sudoers.d/010_pi-nopasswd"
  chmod 0440 "$R/etc/sudoers.d/010_pi-nopasswd"
  ch visudo -cq
  ch systemctl disable userconfig.service >/dev/null 2>&1
  cat > "$R/usr/sbin/bootbob-first-boot" <<'EOF'
#!/bin/sh
# Lets in whoever holds the key in the boot partition. Runs once on each machine.
set -eu
if [ -f /boot/firmware/authorized_keys ]; then
  install -d -m 0700 -o pi -g pi /home/pi/.ssh
  install -m 0600 -o pi -g pi /boot/firmware/authorized_keys /home/pi/.ssh/authorized_keys
fi
touch /var/lib/bootbob-first-boot
EOF
  chmod 755 "$R/usr/sbin/bootbob-first-boot"
  cat > "$R/etc/systemd/system/bootbob-first-boot.service" <<'EOF'
[Unit]
Description=First boot of a BootBob golden image
ConditionPathExists=!/var/lib/bootbob-first-boot
RequiresMountsFor=/boot/firmware /home
Before=ssh.service

[Service]
Type=oneshot
ExecStart=/usr/sbin/bootbob-first-boot

[Install]
WantedBy=multi-user.target
EOF

  # DHCP names each machine: NetworkManager takes the name it's given when there's no
  # static one, and cloud-init leaves it alone.
  rm -f "$R/etc/hostname"
  sed -i "/^127\.0\.1\.1/d" "$R/etc/hosts"
  printf "preserve_hostname: true\n" > "$R/etc/cloud/cloud.cfg.d/90_bootbob.cfg"

  # Pi OS runs it everywhere; only these boards have an EEPROM, and nothing else (a VM) has
  # anything to update.
  mkdir -p "$R/etc/systemd/system/rpi-eeprom-update.service.d"
  printf "[Unit]\nConditionFirmware=|device-tree-compatible(brcm,bcm2711)\nConditionFirmware=|device-tree-compatible(brcm,bcm2712)\n" \
    > "$R/etc/systemd/system/rpi-eeprom-update.service.d/boards.conf"

  install -m 0755 /machine/bootbob-rollout "$R/usr/sbin/bootbob-rollout"
  install -m 0644 /machine/bootbob-rollout.service /machine/bootbob-rollout.timer "$R/etc/systemd/system/"
  ch systemctl enable ssh bootbob-first-boot bootbob-rollout.timer >/dev/null 2>&1
  ch update-initramfs -u -k all >/dev/null

  FW="$R/boot/firmware"
  printf "enable_uart=1\n" >> "$FW/config.txt"
  # Used only when a BootBob image gives no command line of its own, which it must: this
  # one cannot name the disk.
  echo "console=serial0,115200 console=tty1 root=/dev/nbd0p2 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client rootwait" > "$FW/cmdline.txt"
  [ ! -f /keys ] || install -m 0600 /keys "$FW/authorized_keys"
  for f in start4.elf fixup4.dat kernel8.img initramfs8 kernel_2712.img initramfs_2712 bcm2711-rpi-4-b.dtb; do
    [ -f "$FW/$f" ] || { echo "the boot partition has no $f" >&2; exit 1; }
  done
  rm -rf "$R"/var/lib/apt/lists/* "$R"/var/log/apt/* "$R/etc/resolv.conf.dpkg-new"
  KVERS=$(ls "$R/lib/modules" | tr "\n" " ")
  for m in dev/pts dev sys proc etc/resolv.conf; do umount "$R/$m"; done

  # For tools/test-pios-golden.sh: the Pi kernels only run on Pis, so a VM boots this image
  # with Debian's generic kernel, installed in a copy that is thrown away.
  if [ -n "$TEST_KERNEL" ]; then
    cp -a "$R" /work/testroot
    T=/work/testroot
    printf "virtio_pci\nvirtio_net\nvfat\nnls_cp437\nnls_ascii\n" >> "$T/etc/initramfs-tools/modules"
    for m in proc sys dev dev/pts; do mount --bind "/$m" "$T/$m"; done
    rm -f "$T/etc/resolv.conf"
    cp /etc/resolv.conf "$T/etc/resolv.conf"
    chroot "$T" sh -c "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq linux-image-arm64" >/dev/null
    for m in dev/pts dev sys proc; do umount "$T/$m"; done
    KVER=$(ls "$T/boot" | sed -n "s/^vmlinuz-\(.*-arm64\)$/\1/p" | sort -V | tail -1)
    cp "$T/boot/vmlinuz-$KVER" "/out/$NAME.test-vmlinuz"
    cp "$T/boot/initrd.img-$KVER" "/out/$NAME.test-initrd"
    rm -rf "$T"
  fi

  # Unused blocks become holes in the file, so it stays small however big the disk.
  fstrim "$R"
  fstrim "$R/boot/firmware"
  cleanup
  trap - EXIT
  mv "$IMG" "/out/$NAME"
  echo "built /out/$NAME: $KVERS ${SIZE_GIB} GiB"
  exit 0
fi

OUT=${1:?usage: make-pios-golden.sh <out.img> [size-GiB] [authorized_keys-file]}
SIZE_GIB=${2:-32}
KEYS=${3:-}
OUTDIR=$(cd "$(dirname "$OUT")" && pwd)
NAME=$(basename "$OUT")
HERE=$(cd "$(dirname "$0")" && pwd)
[ -z "$KEYS" ] || [ -f "$KEYS" ] || { echo "no such keys file: $KEYS" >&2; exit 1; }
KEYMOUNT=""
[ -n "$KEYS" ] && KEYMOUNT="-v $(cd "$(dirname "$KEYS")" && pwd)/$(basename "$KEYS"):/keys:ro"
[ -z "${PIOS_URL:-}" ] || [ -n "${PIOS_SHA256:-}" ] || { echo "PIOS_URL needs PIOS_SHA256" >&2; exit 1; }

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

# --privileged: loop devices, mounts and a chroot. The container is thrown away afterwards;
# its loop devices are detached on the way out.
# shellcheck disable=SC2086
docker run --rm --privileged --platform linux/arm64 -v "$OUTDIR:/out" -v "$WORK:/tools:ro" \
  -v "$HERE/machine:/machine:ro" -v "$HERE/make-pios-golden.sh:/make.sh:ro" $KEYMOUNT \
  -e NAME="$NAME" -e SIZE_GIB="$SIZE_GIB" -e TEST_KERNEL="${TEST_KERNEL:-}" \
  -e PIOS_URL="${PIOS_URL:-}" -e PIOS_SHA256="${PIOS_SHA256:-}" \
  debian:trixie sh -c "mkdir -p /work && sh -eu /make.sh --inside"
[ -f "$OUT" ] || { echo "the build finished without making $OUT" >&2; exit 1; }
echo "Done: $OUT"
