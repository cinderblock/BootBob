#!/bin/sh
# Builds BootBob's iPXE binaries from a pinned iPXE commit.
#
#   ipxe/build.sh <out-dir> [ipxe-checkout]
#
# Needs: git, make, gcc, perl, liblzma-dev; gcc-aarch64-linux-gnu for the arm64 build
# (skipped with a warning when missing). Writes the files BootBob's ipxe_dir expects:
#   bootbob-bios.kpxe       legacy BIOS (undionly: uses the firmware's PXE stack)
#   bootbob-efi-x64.efi     UEFI x86-64 (snponly: uses the firmware's network stack)
#   bootbob-efi-arm64.efi   UEFI arm64
set -eu

IPXE_REPO=https://github.com/ipxe/ipxe.git
IPXE_COMMIT=70f39d5b0dcf427d0dd143bf5f55c82df401c486 # master, 2026-10-23

here=$(cd "$(dirname "$0")" && pwd)
[ $# -ge 1 ] || { echo "usage: $0 <out-dir> [ipxe-checkout]" >&2; exit 2; }
mkdir -p "$1"
out=$(cd "$1" && pwd)
src=${2:-"$here/.src"}

if [ ! -d "$src/.git" ]; then
  git init -q "$src"
  git -C "$src" remote add origin "$IPXE_REPO"
fi
if ! git -C "$src" cat-file -e "$IPXE_COMMIT^{commit}" 2>/dev/null; then
  git -C "$src" fetch -q --depth 1 origin "$IPXE_COMMIT"
fi
git -C "$src" checkout -q --detach "$IPXE_COMMIT"
cp "$here"/local/*.h "$src/src/config/local/"

jobs=$(nproc 2>/dev/null || echo 4)
build() { make -C "$src/src" -j"$jobs" EMBED="$here/embed.ipxe" "$@"; }

build bin/undionly.kpxe bin-x86_64-efi/snponly.efi
cp "$src/src/bin/undionly.kpxe" "$out/bootbob-bios.kpxe"
cp "$src/src/bin-x86_64-efi/snponly.efi" "$out/bootbob-efi-x64.efi"

if command -v aarch64-linux-gnu-gcc >/dev/null; then
  build CROSS=aarch64-linux-gnu- bin-arm64-efi/snponly.efi
  cp "$src/src/bin-arm64-efi/snponly.efi" "$out/bootbob-efi-arm64.efi"
else
  echo "warning: no aarch64-linux-gnu-gcc, so no arm64 build" >&2
fi

echo "iPXE $IPXE_COMMIT" > "$out/VERSION"
ls -l "$out"
