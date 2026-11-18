# Architecture

## Pieces

Each cluster runs one gateway pod (a Deployment with one replica) in its own namespace. The pod
holds everything zweik8s needs, inside its own network namespace:

| Piece | Where | Purpose |
|---|---|---|
| WireGuard interface `zweik8s` | pod netns, kernel | Encryption; carries only the two `/32` tunnel addresses |
| UDP bridge | `zweik8s` process | Moves encrypted packets between WireGuard and the peer's public endpoint |
| Probe and watchdog | `zweik8s` process | Measures the tunnel, heals stale NAT mappings |
| TCP/UDP relays | `zweik8s` process | One listener per import and per export |
| nftables table `zweik8s` | pod netns | Admits from the tunnel only the probe and declared exports |
| Health server | `127.0.0.1:9090` (or the pod IP with metrics on) | `/livez`, `/readyz`, `/status`, `/metrics` |

Kubernetes objects: the Deployment, a ConfigMap with the runtime config, a Service publishing
the encrypted UDP port (LoadBalancer or NodePort), one ClusterIP Service per import, a
NetworkPolicy, and the key Secret (yours, or an ExternalSecret).

## A connection, end to end

1. An app in cluster A connects to the import Service `postgres:5432`. kube-proxy (or your CNI)
   sends it to gateway A's pod IP on the import's `tunnelPort`.
2. The NetworkPolicy admits it only if the app's pod matches one of the import's `callers`.
3. Gateway A accepts and opens a TCP connection to `169.254.100.2:<tunnelPort>`, the peer's
   tunnel address. That route points at the WireGuard interface.
4. WireGuard encrypts the packets and sends them to the UDP bridge on loopback; the bridge sends
   them to cluster B's public endpoint.
5. In gateway B the bridge hands them to WireGuard, which decrypts them. nftables lets them in
   because `<tunnelPort>` is a declared export.
6. Gateway B resolves `postgres.databases.svc.cluster.local` and connects. From here the
   Service behaves as for any local client.

Both TCP legs that cross the tunnel use BBR congestion control when the kernel offers it (set
per socket, no node change). Payload moves between the two sockets of a session with splice(2)
through a kernel pipe and never enters user space.

UDP works the same way, with one upstream socket per client address. An association expires
after `udpIdleSeconds` without traffic and is re-created every `udpRefreshSeconds`, so the
target name is resolved again.

## Why a UDP bridge

WireGuard updates its idea of the peer's address from every authenticated packet. Behind NAT,
that address is whatever the peer's egress NAT picked, not its public load balancer. After the
peer's pod moves or the NAT forgets the mapping, WireGuard keeps answering a dead address.

So WireGuard never talks to the network directly. It listens on a private port and its peer
endpoint is a loopback socket owned by `zweik8s`:

```text
              kernel WireGuard (port P+1)
                 ▲                  │ encrypted
                 │                  ▼
   public socket :P  ──►  loopback socket  ──►  outgoing socket (ephemeral port)
   (from peer)        (WireGuard's "peer")          │ always to peer.endpoint
                 ▲                                  ▼
                 └──── replies via NAT ◄──── the peer's public endpoint
```

- Everything WireGuard sends goes to the configured `peer.endpoint`, from an ephemeral port.
- Everything that arrives (on the public port or the outgoing socket) goes to WireGuard.
- Three independent tasks forward the three directions on a multi-threaded runtime, up to 64
  packets per syscall (recvmmsg/sendmmsg), with 4 MiB socket buffers.

## Probe and recovery

Every `probeIntervalSeconds` (5) each gateway sends a 17-byte nonce to the peer's tunnel
address on the control port (19999); the peer echoes it. The reply gives `zweik8s_peer_up` and
`zweik8s_peer_rtt_seconds`.

If no reply arrives for `peerTimeoutSeconds` (45), the gateway:

1. resolves `peer.endpoint` again (it may be a DNS name),
2. opens a new outgoing socket, so the NAT sees a new flow,
3. removes and re-adds the WireGuard peer, forcing a fresh handshake.

Resets are at least 12 seconds apart, so a handshake in progress (retried every 5 s) can finish.
Readiness never depends on the peer. A gateway waiting for its peer stays Ready, so its load
balancer keeps routing to it and the peer can reach it.

## Lifecycle

- Start: bind the UDP sockets, create the interface (removing one left by a crashed container),
  set keys and peer, install the nftables table atomically, add the route, start listeners, go
  Ready.
- SIGTERM: go NotReady, stop accepting, wait up to `drainSeconds` for open sessions, remove the
  interface and the table.
- The Deployment uses `Recreate`: two gateways with the same key would fight over the session.

## Design choices

- **Kernel WireGuard, not user space.** It is in every current kernel, it is fast, and the pod
  only needs `NET_ADMIN` in its own namespace. No host access.
- **A relay, not routing.** Routing remote Service CIDRs means touching the CNI, kube-proxy and
  every node, and breaks on overlapping CIDRs. Relaying at the gateway needs none of that and
  makes the allow-list explicit.
- **DNS at the exporting side.** The importer never learns remote ClusterIPs, so Service
  changes on the exporting side need no update anywhere.
- **No control plane.** Each side is a Helm release with static values. The two sides agree on
  tunnel addresses, endpoints and `tunnelPort`s, and nothing else.
