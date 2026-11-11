#!/bin/sh
# Shows why BootBob binds each boot interface's sockets to the interface (SO_BINDTODEVICE),
# not only to its address. Linux is a weak-host system: a packet addressed to one interface's
# address is accepted when it arrives on another. Here a peer behind va1 reaches a server on
# dum0's address, over TCP and UDP; bound to dum0, the same server never sees it.
#
# Needs root. Makes a dummy interface, a veth pair and a network namespace, and removes them.
# Usage: sudo tools/test-weak-host.sh
set -e
cleanup() { ip netns del bbpeer 2>/dev/null || true; ip link del va1 2>/dev/null || true; ip link del dum0 2>/dev/null || true; }
cleanup
ip link add dum0 type dummy; ip addr add 10.9.2.1/24 dev dum0; ip link set dum0 up
ip netns add bbpeer
ip link add va1 type veth peer name vb1
ip link set vb1 netns bbpeer
ip addr add 10.9.1.1/24 dev va1; ip link set va1 up
ip -n bbpeer addr add 10.9.1.2/24 dev vb1; ip -n bbpeer link set vb1 up; ip -n bbpeer link set lo up
ip -n bbpeer route add 10.9.2.0/24 via 10.9.1.1
cat > /tmp/bbserve.py <<'PY'
import socket, sys
dev = sys.argv[1].encode() if sys.argv[1] != "-" else None
t = socket.socket(); t.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
for s in (t, u):
    if dev: s.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, dev)
t.bind(("10.9.2.1", 7001)); t.listen(1); u.bind(("10.9.2.1", 7002))
t.settimeout(3); u.settimeout(3)
try: t.accept(); print(sys.argv[1], "TCP from va1: accepted")
except Exception as e: print(sys.argv[1], "TCP from va1: not accepted:", e)
try: u.recvfrom(100); print(sys.argv[1], "UDP from va1: received")
except Exception as e: print(sys.argv[1], "UDP from va1: not received:", e)
PY
for dev in - dum0; do
  python3 /tmp/bbserve.py $dev & srv=$!
  sleep 0.5
  ip netns exec bbpeer python3 -c '
import socket
c = socket.socket(); c.settimeout(2)
try: c.connect(("10.9.2.1", 7001)); print("  client: TCP connected")
except Exception as e: print("  client: TCP failed:", e)
socket.socket(socket.AF_INET, socket.SOCK_DGRAM).sendto(b"x", ("10.9.2.1", 7002))'
  wait $srv
done
cleanup
