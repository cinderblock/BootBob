#!/usr/bin/env bash
# End-to-end test: real firmware netboots from BootBob in QEMU, on a private bridge.
#
#   sudo e2e/run.sh <bootbob-binary> <ipxe-dir>
#
# Needs root (bridge, tap, TFTP port 69, the DHCP listener's packet socket), qemu-system-x86,
# ovmf and dnsmasq. Meant for a throwaway CI VM: it creates bridge bb-e2e and runs dnsmasq
# as the DHCP server on it, standing in for UniFi with next-server plus one boot file name.
# dnsmasq lives in its own network namespace on a veth into the bridge, so its replies
# reach BootBob from outside, as a real gateway's do. The bridge has no uplink, so none of
# this reaches a real network.
#
# First, the network check must pass for BIOS and UEFI (its probes get dnsmasq's offers).
# Then, for each firmware (SeaBIOS, then OVMF) it checks that:
#   1. the machine reaches BootBob's fallback menu, which proves TFTP served the right iPXE
#      build under the single file name (default_firmware is deliberately wrong, so the
#      DHCP listener's arch detection has to have worked);
#   2. BootBob recorded the right firmware;
#   3. assigning "Power off" in the API while it waits at the menu powers the VM off.
set -euo pipefail

bootbob=$(realpath "$1")
ipxe=$(realpath "$2")
work=$(mktemp -d)
br=bb-e2e
net=192.168.77
api=http://$net.1:8067/api
pids=()

cleanup() {
  status=$?
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  ip link del bb-tap0 2>/dev/null || true
  ip link del bb-veth0 2>/dev/null || true
  ip netns del bb-dhcp 2>/dev/null || true
  ip link del "$br" 2>/dev/null || true
  if [ $status -ne 0 ]; then
    for f in "$work"/*.log; do echo "==== $f"; tail -n 80 "$f"; done
  fi
  exit $status
}
trap cleanup EXIT

ip link add "$br" type bridge
ip addr add "$net.1/24" dev "$br"
ip link set "$br" up
ip tuntap add bb-tap0 mode tap
ip link set bb-tap0 master "$br" up

ip netns add bb-dhcp
ip link add bb-veth0 type veth peer name bb-veth1
ip link set bb-veth1 netns bb-dhcp
ip link set bb-veth0 master "$br" up
ip -n bb-dhcp addr add "$net.2/24" dev bb-veth1
ip -n bb-dhcp link set bb-veth1 up
ip -n bb-dhcp link set lo up

ip netns exec bb-dhcp dnsmasq --no-daemon --conf-file=/dev/null --port=0 --interface=bb-veth1 --bind-interfaces \
  --dhcp-range="$net.50,$net.99,1h" --dhcp-boot="bootbob,,$net.1" --log-dhcp \
  --dhcp-leasefile="$work/leases" >"$work/dnsmasq.log" 2>&1 &
pids+=($!)

cat >"$work/bootbob.toml" <<EOF
data_dir = "$work/data"
http = "$net.1:8067"
tftp = "$net.1:69"
interface = "$br"
ipxe_dir = "$ipxe"
default_firmware = "efi-arm64"
EOF
RUST_LOG=info,bootbob=debug "$bootbob" --config "$work/bootbob.toml" >"$work/bootbob.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do curl -sf "$api/status" >/dev/null && break; sleep 0.2; done
curl -sf "$api/status"; echo

# Wait until `jq filter` on the device is true. $1 mac, $2 filter, $3 seconds, $4 what.
wait_for() {
  local deadline=$((SECONDS + $3))
  while ((SECONDS < deadline)); do
    if curl -sf "$api/devices/$1" | jq -e "$2" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  echo "FAIL: $4 (after $3 s)"; curl -s "$api/devices/$1" | jq . || true
  return 1
}

# What actually crosses the bridge, for when a check fails.
tcpdump -lnei "$br" 'udp and (port 67 or port 68 or port 69)' >"$work/tcpdump.log" 2>&1 &
pids+=($!)
sleep 1

echo "==== network check"
report=$(curl -sf -X POST "$api/doctor")
echo "$report" | jq -r '.checks[] | "\(.status)\t\(.title): \(.detail)"'
for id in dhcp-efi-x64 dhcp-bios dhcp-rogue listener tftp; do
  echo "$report" | jq -e --arg id "$id" '.checks[] | select(.id == $id) | .status == "ok"' >/dev/null \
    || { echo "FAIL: network check $id"; exit 1; }
done
echo "ok: the network check sees dnsmasq's boot options for BIOS and UEFI"

accel=tcg
[ -w /dev/kvm ] && accel=kvm

run() { # name mac firmware-slug qemu-args...
  local name=$1 mac=$2 slug=$3; shift 3
  echo "==== $name ($accel)"
  qemu-system-x86_64 -accel "$accel" -m 512 -display none -serial "file:$work/$name-serial.log" \
    -netdev tap,id=n0,ifname=bb-tap0,script=no,downscript=no \
    -device "virtio-net-pci,netdev=n0,mac=$mac" -no-reboot "$@" >"$work/$name-qemu.log" 2>&1 &
  local qemu=$!
  pids+=("$qemu")

  wait_for "$mac" '.state == "at_menu"' 240 "$name reached the fallback menu"
  wait_for "$mac" ".firmware == \"$slug\"" 5 "$name recorded as $slug"  echo "ok: $name is waiting at the menu as $slug"

  curl -sf -X PUT -H 'content-type: application/json' -d '{"image":"poweroff","once":true}' \
    "$api/devices/$mac/assignment" >/dev/null
  local deadline=$((SECONDS + 90))
  while kill -0 "$qemu" 2>/dev/null && ((SECONDS < deadline)); do sleep 1; done
  if kill -0 "$qemu" 2>/dev/null; then echo "FAIL: $name didn't power off after the assignment"; return 1; fi
  wait_for "$mac" '.last_boot.image == "poweroff" and .assignment == null' 5 "$name recorded the power-off"
  echo "ok: $name powered off from the web API"
}

run seabios 52:54:00:00:00:01 bios -boot n
run ovmf 52:54:00:00:00:02 efi-x64 -bios /usr/share/ovmf/OVMF.fd

echo "==== events"
curl -sf "$api/events" | jq -r '.[] | "\(.mac // "-") \(.kind): \(.message)"'
echo "PASS"
