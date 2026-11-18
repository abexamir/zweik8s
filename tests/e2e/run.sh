#!/usr/bin/env bash
# End-to-end test on one Linux host: two network namespaces ("site a" and "site b") joined by
# a veth pair, each running a gateway with kernel WireGuard. Needs root, ip, wg, nft, iperf3,
# python3. Usage: tests/e2e/run.sh [path/to/zweik8s]   (BENCH=1 adds throughput runs)
set -euo pipefail
umask 077

BIN=$(realpath "${1:-target/release/zweik8s}")
WORK=$(mktemp -d)
A=zwk-a B=zwk-b
PASS=0
FAIL=0

cleanup() {
  for ns in $A $B; do
    ip netns pids $ns 2>/dev/null | xargs -r kill -9 2>/dev/null || true
    ip netns del $ns 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT
cleanup 2>/dev/null; WORK=$(mktemp -d)

ok() { PASS=$((PASS + 1)); echo "  ok   $*"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }
check() { local what=$1; shift; if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi; }
in_a() { ip netns exec $A "$@"; }
in_b() { ip netns exec $B "$@"; }

# ── topology ─────────────────────────────────────────────────────────────────
ip netns add $A
ip netns add $B
ip link add veth-a netns $A type veth peer name veth-b netns $B
in_a ip addr add 10.99.0.1/24 dev veth-a
in_b ip addr add 10.99.0.2/24 dev veth-b
for ns in $A $B; do ip -n $ns link set lo up; done
in_a ip link set veth-a up
in_b ip link set veth-b up

wg genkey > "$WORK/a.key"; wg pubkey < "$WORK/a.key" > "$WORK/a.pub"
wg genkey > "$WORK/b.key"; wg pubkey < "$WORK/b.key" > "$WORK/b.pub"

# Site b exports a TCP echo, a UDP echo and iperf3; site a imports them.
cat > "$WORK/a.json" <<EOF
{"localAddress":"169.254.77.1","peerAddress":"169.254.77.2","peerEndpoint":"10.99.0.2:51822",
 "listenPort":51822,"keys":{"privateKey":"$WORK/a.key","peerPublicKey":"$WORK/b.pub"},
 "health":{"bind":"127.0.0.1:9090"},
 "tuning":{"probeIntervalSeconds":1,"peerTimeoutSeconds":6,"drainSeconds":3},
 "listeners":[
  {"name":"echo","bind":"pod","protocol":"TCP","listenPort":20001,"targetHost":"169.254.77.2","targetPort":20001},
  {"name":"uecho","bind":"pod","protocol":"UDP","listenPort":20002,"targetHost":"169.254.77.2","targetPort":20002},
  {"name":"iperf","bind":"pod","protocol":"TCP","listenPort":20003,"targetHost":"169.254.77.2","targetPort":20003}]}
EOF
cat > "$WORK/b.json" <<EOF
{"localAddress":"169.254.77.2","peerAddress":"169.254.77.1","peerEndpoint":"10.99.0.1:51822",
 "listenPort":51822,"keys":{"privateKey":"$WORK/b.key","peerPublicKey":"$WORK/a.pub"},
 "health":{"bind":"127.0.0.1:9090"},
 "tuning":{"probeIntervalSeconds":1,"peerTimeoutSeconds":6,"drainSeconds":3},
 "listeners":[
  {"name":"echo","bind":"tunnel","protocol":"TCP","listenPort":20001,"targetHost":"localhost","targetPort":7001},
  {"name":"uecho","bind":"tunnel","protocol":"UDP","listenPort":20002,"targetHost":"localhost","targetPort":7002},
  {"name":"iperf","bind":"tunnel","protocol":"TCP","listenPort":20003,"targetHost":"localhost","targetPort":5201}]}
EOF

cat > "$WORK/echo.py" <<'EOF'
import socket, socketserver, threading
class T(socketserver.StreamRequestHandler):
    def handle(self):
        while data := self.request.recv(1 << 16):
            self.request.sendall(data)
class U(socketserver.BaseRequestHandler):
    def handle(self):
        self.request[1].sendto(self.request[0], self.client_address)
socketserver.ThreadingTCPServer.allow_reuse_address = True
threading.Thread(target=socketserver.ThreadingTCPServer(("127.0.0.1", 7001), T).serve_forever, daemon=True).start()
# A service that is NOT exported, listening on every address of site b.
threading.Thread(target=socketserver.ThreadingTCPServer(("0.0.0.0", 7777), T).serve_forever, daemon=True).start()
socketserver.UDPServer(("127.0.0.1", 7002), U).serve_forever()
EOF
in_b python3 "$WORK/echo.py" & sleep 0.3
in_b iperf3 -s -D -B 127.0.0.1 -p 5201

start() { # side
  local ns=$1 side=$2
  POD_IP=10.99.0.$([ "$side" = a ] && echo 1 || echo 2) ip netns exec "$ns" "$BIN" run --config "$WORK/$side.json" \
    >>"$WORK/$side.log" 2>&1 &
  echo $! > "$WORK/$side.pid"
}
wait_up() { # ns; WireGuard retries a lost first handshake after 5 s, so allow a few
  for _ in $(seq 1 100); do
    ip netns exec "$1" "$BIN" status 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["ready"] and d["peerReplyAgeSeconds"] is not None and d["peerReplyAgeSeconds"] < 3 else 1)' && return 0
    sleep 0.2
  done
  return 1
}

start $A a
start $B b
T0=$(date +%s.%N)
echo "tunnel"
check "site a ready and hears the peer" wait_up $A
check "site b ready and hears the peer" wait_up $B
echo "       (tunnel up $(python3 -c "print(round($(date +%s.%N) - $T0, 1))") s after start)"
check "WireGuard handshake completed" bash -c "in_a() { ip netns exec $A \"\$@\"; }; [ \$(ip netns exec $A wg show zweik8s latest-handshakes | awk '{print \$2}') -gt 0 ]"

cat > "$WORK/tcp_check.py" <<'EOF'
import hashlib, os, socket, sys, threading
size = int(sys.argv[2])
data = os.urandom(size)
s = socket.create_connection((sys.argv[1], 20001), timeout=30)
threading.Thread(target=lambda: (s.sendall(data), s.shutdown(socket.SHUT_WR)), daemon=True).start()
got = bytearray()
while chunk := s.recv(1 << 20):
    got += chunk
sys.exit(0 if hashlib.sha256(got).digest() == hashlib.sha256(data).digest() else 1)
EOF
cat > "$WORK/udp_check.py" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(2)
for i in range(200):
    msg = bytes([i % 256]) * 1200
    s.sendto(msg, (sys.argv[1], 20002))
    assert s.recv(2048) == msg
EOF
cat > "$WORK/denied.py" <<'EOF'
import socket, sys
try:
    socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=2).close()
    sys.exit(1)  # connected: firewall failed
except OSError:
    sys.exit(0)
EOF

echo "data path"
check "TCP: 64 MiB round trip is byte-identical" in_a python3 "$WORK/tcp_check.py" 10.99.0.1 $((64 << 20))
check "TCP: 50 parallel sessions" bash -c "for i in \$(seq 50); do ip netns exec $A python3 $WORK/tcp_check.py 10.99.0.1 65536 & done; wait"
check "UDP: 200 datagrams echo intact" in_a python3 "$WORK/udp_check.py" 10.99.0.1

echo "isolation"
check "peer cannot reach an unexported port on the tunnel address" in_a python3 "$WORK/denied.py" 169.254.77.2 7777
check "peer cannot reach the import port from the tunnel side" in_b python3 "$WORK/denied.py" 169.254.77.1 20001
check "no forwarding through the gateway" bash -c "! ip netns exec $A ping -c1 -W1 -I zweik8s 10.99.0.2 >/dev/null"

echo "observability"
check "/metrics exposes counters" bash -c "ip netns exec $A $BIN probe /metrics >/dev/null && ip netns exec $A python3 -c \"import urllib.request as u; t=u.urlopen('http://127.0.0.1:9090/metrics').read().decode(); assert 'zweik8s_peer_up 1' in t and 'listener=\\\"echo\\\"' in t\""
check "/readyz is 200" in_a "$BIN" probe /readyz

echo "recovery"
# Black-hole the encrypted path for longer than peerTimeoutSeconds, then restore it.
in_a nft add table inet blackhole
in_a nft add chain inet blackhole out '{ type filter hook output priority 0; }'
in_a nft add rule inet blackhole out udp dport 51822 drop
sleep 9
in_a nft delete table inet blackhole
check "transport reset after the peer went silent" bash -c "ip netns exec $A $BIN status | grep -q '\"transportResets\":[1-9]'"
check "tunnel recovers after packet loss" wait_up $A
check "TCP works after recovery" in_a python3 "$WORK/tcp_check.py" 10.99.0.1 1048576
kill -9 "$(cat "$WORK/b.pid")"; sleep 0.5
start $B b
check "peer restart: site b back" wait_up $B
check "peer restart: site a hears it again" wait_up $A
check "TCP works after a peer restart" in_a python3 "$WORK/tcp_check.py" 10.99.0.1 1048576

echo "shutdown"
in_a python3 - <<EOF &
import socket, time
s = socket.create_connection(("10.99.0.1", 20001)); s.sendall(b"x"); s.recv(1); time.sleep(1.5); s.close()
EOF
sleep 0.3
kill -TERM "$(cat "$WORK/a.pid")"
check "SIGTERM: drains then exits within drainSeconds" bash -c "for i in \$(seq 50); do kill -0 $(cat "$WORK/a.pid") 2>/dev/null || exit 0; sleep 0.1; done; exit 1"
check "SIGTERM: interface removed" bash -c "! ip netns exec $A ip link show zweik8s"
check "shutdown waited for the open session" grep -q '"open_sessions":"0"' "$WORK/a.log"
start $A a
check "restart after clean shutdown" wait_up $A

if [ "${BENCH:-0}" = 1 ]; then
  echo "throughput (iperf3 through the tunnel, 10 s each)"
  for args in "" "-R" "-P 8" "-P 8 -R"; do
    in_a iperf3 -c 10.99.0.1 -p 20003 -t 10 $args -J > "$WORK/bench.json"
    python3 -c "import json; e=json.load(open('$WORK/bench.json'))['end']; print(f'  iperf3 $args'.ljust(24), round(e['sum_received']['bits_per_second']/1e9, 2), 'Gbit/s')"
  done
fi

echo "logs: site a $(wc -l < "$WORK/a.log") lines, site b $(wc -l < "$WORK/b.log") lines"
[ "$FAIL" = 0 ] || { echo "--- site a log"; tail -20 "$WORK/a.log"; echo "--- site b log"; tail -20 "$WORK/b.log"; }
echo "passed $PASS, failed $FAIL"
[ "$FAIL" = 0 ]
