#!/usr/bin/env bash
# A real rollout: real firmware netbooting real kernels from BootBob, on a private bridge, with
# the check-in timer in the image doing the rebooting.
#
#   sudo tools/test-rollout.sh <bootbob-binary> <ipxe-dir> <make-pc-golden image>
#
# Needs root (bridge, taps, TFTP, the DHCP listener), qemu-system-x86, dnsmasq, jq, curl, ssh and
# e2fsprogs. Works on copies of the image, each given a throwaway key to log in with. Meant for
# a throwaway container or CI VM: the bridge has no uplink, so none of this reaches a real
# network. Two VMs follow a track through:
#
#   1. a new version: one machine at a time, each on a new root, the old overlay left alone;
#   2. a rollback: both back, and the first has what it wrote on that root;
#   3. a busy machine: skipped, and it takes its turn once it's idle;
#   4. a broken version: the first machine comes back unhealthy, the track halts, that machine
#      takes itself back, and the second never moves.
#
# Timers are sped up in the test's images. Everything else is as shipped.
set -euo pipefail

bootbob=$(realpath "$1")
ipxe=$(realpath "$2")
golden=$(realpath "$3")
work=$(mktemp -d)
key=$work/key
br=bb-roll
net=192.168.78
url=http://$net.1:8067
api=$url/api
pids=()
macs=(52:54:00:b0:b0:01 52:54:00:b0:b0:02)

cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    # What the machines themselves think, while they're still up.
    for m in "${macs[@]}"; do
      echo "==== inside $m"
      timeout 30 bash -c "$(declare -f vm dev); api=$api; key=$key; vm $m 'cat /proc/cmdline; ip route; systemctl is-system-running; systemctl --failed --no-legend; systemctl status --no-pager bootbob-rollout.timer bootbob-rollout.service; journalctl --no-pager -n 30 -u bootbob-rollout.service'" || true
    done
  fi
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  ip link del bb-rtap0 2>/dev/null || true
  ip link del bb-rtap1 2>/dev/null || true
  ip link del bb-rveth0 2>/dev/null || true
  ip netns del bb-rdhcp 2>/dev/null || true
  ip link del "$br" 2>/dev/null || true
  if [ $status -ne 0 ]; then
    for f in "$work"/*.log; do echo "==== $f"; tail -n 60 "$f"; done
    curl -sf "$api/events?limit=60" | jq -r '.[] | "\(.mac // "-") \(.kind): \(.message)"' || true
  fi
  rm -rf "$work"
  exit $status
}
trap cleanup EXIT

say() { echo "==== $*"; }
ok() { echo "ok: $*"; }
die() {
  echo "FAIL: $*"
  exit 1
}

# ---- The versions ----

# The root partition of a make-pc-golden.sh image.
root=$((257 * 1024 * 1024))
fs() { # image debugfs-command
  debugfs -w -R "$2" "$1?offset=$root" 2>&1 | grep -E 'not found|Could not|File exists|error' && die "debugfs: $2 on $1" || true
}

# A copy of the golden image, stamped with its version, checking in every 15 s.
version() { # name
  local img=$work/data/images/$1.img
  cp --sparse=always "$golden" "$img"
  echo "$1" >"$work/stamp"
  fs "$img" "write $work/stamp /etc/bootbob-version"
  # An image built with keys already has both; they aren't the ones this test can use.
  debugfs -w -R "mkdir /root/.ssh" "$img?offset=$root" >/dev/null 2>&1 || true
  debugfs -w -R "rm /root/.ssh/authorized_keys" "$img?offset=$root" >/dev/null 2>&1 || true
  fs "$img" "write $key.pub /root/.ssh/authorized_keys"
  # An empty value clears every timer in the section, OnBootSec included. AccuracySec: by
  # default systemd may run a timer up to a minute late.
  printf '[Timer]\nOnUnitActiveSec=\nOnBootSec=15s\nOnUnitActiveSec=15s\nRandomizedDelaySec=2s\nAccuracySec=1s\n' >"$work/fast.conf"
  fs "$img" "mkdir /etc/systemd/system/bootbob-rollout.timer.d"
  fs "$img" "write $work/fast.conf /etc/systemd/system/bootbob-rollout.timer.d/fast.conf"
}

mkdir -p "$work/data/images"
ssh-keygen -q -t ed25519 -N "" -f "$key"
say "making versions"
version fleet-1
version fleet-2
# The third has a service that fails, the way a bad update would.
version fleet-3
printf '[Service]\nExecStart=/bin/false\n[Install]\nWantedBy=multi-user.target\n' >"$work/broken.service"
fs "$work/data/images/fleet-3.img" "write $work/broken.service /etc/systemd/system/broken.service"
fs "$work/data/images/fleet-3.img" "symlink /etc/systemd/system/multi-user.target.wants/broken.service /etc/systemd/system/broken.service"
sha256sum "$work"/data/images/*.img >"$work/sums"

# ---- The network ----

ip link add "$br" type bridge
ip addr add "$net.1/24" dev "$br"
ip link set "$br" up
for i in 0 1; do
  ip tuntap add "bb-rtap$i" mode tap
  ip link set "bb-rtap$i" master "$br" up
done
ip netns add bb-rdhcp
ip link add bb-rveth0 type veth peer name bb-rveth1
ip link set bb-rveth1 netns bb-rdhcp
ip link set bb-rveth0 master "$br" up
ip -n bb-rdhcp addr add "$net.2/24" dev bb-rveth1
ip -n bb-rdhcp link set bb-rveth1 up
ip -n bb-rdhcp link set lo up
ip netns exec bb-rdhcp dnsmasq --no-daemon --conf-file=/dev/null --port=0 --interface=bb-rveth1 --bind-interfaces \
  --dhcp-range="$net.50,$net.99,1h" --dhcp-boot="bootbob,,$net.1" --dhcp-leasefile="$work/leases" >"$work/dnsmasq.log" 2>&1 &
pids+=($!)

cat >"$work/bootbob.toml" <<EOF
data_dir = "$work/data"
http = "$net.1:8067"
tftp = "$net.1:69"
nbd = "$net.1:10809"
interface = "$br"
ipxe_dir = "$ipxe"
public_url = "$url"
mdns = false
EOF
RUST_LOG=info "$bootbob" --config "$work/bootbob.toml" >"$work/bootbob.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do curl -sf "$api/status" >/dev/null && break; sleep 0.2; done

# ---- Helpers ----

put() { curl -fsS -X PUT -H 'content-type: application/json' -d "$2" "$api/$1" >/dev/null; }
post() { curl -fsS -X POST -H 'content-type: application/json' -d "${2:-{\}}" "$api/$1" >/dev/null; }
dev() { curl -fsS "$api/devices/$1"; }
track() { curl -fsS "$api/tracks" | jq '.[0]'; }

wait_dev() { # mac jq-filter seconds what
  local deadline=$((SECONDS + $3))
  until dev "$1" | jq -e "$2" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || { dev "$1" | jq .; die "$4 (after $3 s)"; }
    sleep 2
  done
}

vm() { # mac command
  local ip
  ip=$(dev "$1" | jq -r .ip)
  ssh -q -i "$key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o BatchMode=yes "root@$ip" "$2"
}

# The version whose root a machine is really running, asked of the machine.
running() { vm "$1" 'cat /etc/bootbob-version'; }

settled() { # mac image what
  wait_dev "$1" ".moving == null and .last_boot.image == \"$2\" and .check.health == \"running\" and .check.at > .last_boot.at" 240 "$3"
  [ "$(running "$1")" = "$2" ] || die "$3: BootBob says $2, the machine says $(running "$1")"
}

accel=tcg
[ -w /dev/kvm ] && accel=kvm
start() { # index
  # No -no-reboot: the machines reboot themselves, back through the firmware's netboot.
  qemu-system-x86_64 -accel "$accel" -smp 2 -m 1024 -display none -serial "file:$work/vm$1-serial.log" \
    -netdev "tap,id=n0,ifname=bb-rtap$1,script=no,downscript=no" \
    -device "virtio-net-pci,netdev=n0,mac=${macs[$1]}" -boot n >"$work/vm$1-qemu.log" 2>&1 &
  pids+=($!)
}

# ---- The fleet ----

cmdline="root=/dev/nbd0p2 nbdroot={server},{disk},nbd0 ip=dhcp NBDCLIENT=/usr/sbin/bootbob-nbd-client bootbob.url={bootbob} console=ttyS0,115200"
put images/fleet-1 "$(jq -n --arg c "$cmdline" '{title:"Fleet", disk:{source:"fleet-1.img", initrd:"initrd.img", cmdline:$c, cow:true}}')"
put tracks/fleet '{"title":"Fleet","image":"fleet-1"}'
# A window that isn't now, so nothing moves until "Run now", and failures show quickly.
settings=$(curl -fsS "$api/settings" | jq --arg s "$(date -u -d '+12 hours' +%H:%M)" '.rollout_start = $s | .rollout_health_minutes = 3')
put settings "$settings"
put "devices/${macs[0]}/assignment" '{"track":"fleet"}'
put "devices/${macs[1]}/assignment" '{"track":"fleet"}'

say "two machines boot version 1 ($accel)"
start 0
start 1
settled "${macs[0]}" fleet-1 "machine 0 didn't come up healthy"
settled "${macs[1]}" fleet-1 "machine 1 didn't come up healthy"
vm "${macs[0]}" 'echo mine >/root/state && sync'
ok "both up on version 1, reporting running; machine 0 wrote a file"

overlay=$work/data/overlays/$(echo "${macs[0]}" | tr : -)

say "a new version: one at a time"
post tracks/fleet/versions '{"source":"fleet-2.img"}'
sleep 35
dev "${macs[0]}" | jq -e '.assignment.image == "fleet-1"' >/dev/null || die "it moved before the window"
ok "published, and nothing moves outside the window"
post tracks/fleet/run
wait_dev "${macs[0]}" '.moving != null' 45 "nothing started"
dev "${macs[1]}" | jq -e '.moving == null and .assignment.image == "fleet-1"' >/dev/null || die "two at once"
settled "${macs[0]}" fleet-2 "machine 0 didn't move"
vm "${macs[0]}" 'test ! -e /root/state' || die "the new root isn't new"
ok "machine 0 rebooted itself onto version 2, a fresh root"
# Only now: a running system writes to its root right up to shutdown.
before=$(cat "$overlay/fleet-1.ovl" "$overlay/fleet-1.map" | sha256sum)
settled "${macs[1]}" fleet-2 "machine 1 didn't follow"
ok "machine 1 went after it"
[ "$(cat "$overlay/fleet-1.ovl" "$overlay/fleet-1.map" | sha256sum)" = "$before" ] || die "the old overlay changed"
ok "the overlay it left is byte for byte what it was"

say "rollback"
post tracks/fleet/release '{"image":"fleet-1","now":true}'
settled "${macs[0]}" fleet-1 "machine 0 didn't go back"
settled "${macs[1]}" fleet-1 "machine 1 didn't go back"
[ "$(vm "${macs[0]}" 'cat /root/state')" = mine ] || die "machine 0's state didn't come back"
ok "both back on version 1, and machine 0 has what it wrote"

say "a busy machine"
vm "${macs[0]}" 'mkdir -p /etc/bootbob && printf "#!/bin/sh\necho a job\n" >/etc/bootbob/busy && chmod +x /etc/bootbob/busy'
wait_dev "${macs[0]}" '.check.busy == "a job"' 60 "the busy hook wasn't reported"
post tracks/fleet/release '{"image":"fleet-2","now":true}'
settled "${macs[1]}" fleet-2 "the idle machine didn't go first"
dev "${macs[0]}" | jq -e '.assignment.image == "fleet-1" and .last_boot.image == "fleet-1"' >/dev/null || die "a busy machine moved"
ok "machine 0 said it was busy, and machine 1 went ahead of it"
vm "${macs[0]}" 'rm /etc/bootbob/busy'
settled "${macs[0]}" fleet-2 "machine 0 didn't take its turn"
ok "idle again, machine 0 took its turn"

say "a login counts as busy"
ip=$(dev "${macs[1]}" | jq -r .ip)
ssh -q -i "$key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes "root@$ip" 'sleep 75' &
session=$!
wait_dev "${macs[1]}" '.check.busy == "login"' 70 "a login wasn't reported"
wait "$session"
ok "a session open on machine 1 was reported"

say "a broken version"
post tracks/fleet/versions '{"source":"fleet-3.img"}'
post tracks/fleet/run
first=
for _ in $(seq 40); do
  for m in "${macs[@]}"; do dev "$m" | jq -e '.moving != null' >/dev/null && first=$m; done
  [ -n "$first" ] && break
  sleep 2
done
[ -n "$first" ] || die "nothing started"
other=${macs[0]}
[ "$first" = "$other" ] && other=${macs[1]}
for _ in $(seq 120); do
  track | jq -e '.halted != null' >/dev/null && break
  sleep 5
done
track | jq -e '.halted != null' >/dev/null || die "a machine stayed degraded and nothing halted"
track | jq -r '.halted.why'
settled "$first" fleet-2 "the failed machine didn't take itself back"
dev "$other" | jq -e '.last_boot.image == "fleet-2" and .assignment.image == "fleet-2"' >/dev/null || die "the second machine moved"
ok "the track halted, the machine took itself back, and the other never left"
sleep 35
dev "$other" | jq -e '.moving == null' >/dev/null || die "a halted track moved a machine"

say "the images"
sha256sum -c "$work/sums" >/dev/null || die "a golden image changed"
ok "every version's disk is unchanged"

curl -fsS "$api/events?limit=500" | jq -r '.[] | select(.kind == "rollout" or .kind == "error") | "\(.mac // "-") \(.message)"'
echo PASS
