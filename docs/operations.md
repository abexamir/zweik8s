# Operations

## Is it working?

```sh
kubectl -n zweik8s exec deploy/zweik8s -- zweik8s status
```

```json
{"version":"0.1.0","ready":true,"peerReplyAgeSeconds":1.2,"peerRttMilliseconds":5.4,
 "handshakeAgeSeconds":37,"transportResets":0,
 "listeners":[{"name":"postgres","protocol":"tcp","kind":"import","active":3,"accepted":120,
   "rejected":0,"failed":0,"bytesForward":1834221,"bytesReverse":99120331}]}
```

- `peerReplyAgeSeconds` below about 5: the tunnel is up end to end. `null` means the peer has
  not answered yet.
- `handshakeAgeSeconds` below about 180: WireGuard is rekeying normally.
- `failed` growing on an export: the target Service is down or its name does not resolve.
- `rejected` growing: `tuning.maxSessions` reached.

Logs are JSON lines: `kubectl -n zweik8s logs deploy/zweik8s`.

## Metrics

With `metrics.enabled`, `/metrics` serves:

| Metric | Type | Meaning |
|---|---|---|
| `zweik8s_peer_up` | gauge | 1 if the peer answered a probe in the last 15 s |
| `zweik8s_peer_rtt_seconds` | gauge | Round trip through the tunnel |
| `zweik8s_peer_reply_age_seconds` | gauge | Since the last probe reply (-1: never) |
| `zweik8s_wireguard_handshake_age_seconds` | gauge | Since the last handshake (-1: never) |
| `zweik8s_transport_resets_total` | counter | Watchdog resets |
| `zweik8s_encrypted_{packets,bytes}_total{direction}` | counter | Through the UDP bridge |
| `zweik8s_bridge_errors_total` | counter | Encrypted packets that could not be sent |
| `zweik8s_tunnel_bytes_total{direction}` | counter | Decrypted bytes on the interface |
| `zweik8s_sessions_active{listener,protocol,kind}` | gauge | Open sessions |
| `zweik8s_sessions_total`, `_rejected_total`, `_failed_total` | counter | Per listener |
| `zweik8s_forward_bytes_total`, `zweik8s_reverse_bytes_total` | counter | Per listener |

Suggested alerts:

```yaml
- alert: Zweik8sPeerDown
  expr: zweik8s_peer_up == 0
  for: 2m
- alert: Zweik8sTargetFailing
  expr: increase(zweik8s_sessions_failed_total[10m]) > 0
- alert: Zweik8sSessionsRejected
  expr: increase(zweik8s_sessions_rejected_total[10m]) > 0
- alert: Zweik8sResetting
  expr: increase(zweik8s_transport_resets_total[30m]) > 2
```

## Changing exports and imports

Add the export on one side and the import on the other with the same `tunnelPort`, then upgrade
both releases. Each config change restarts that side's gateway (a checksum annotation), which
drops its open connections, so change both sides in one maintenance window. A tunnel survives
either side restarting; recovery takes a few seconds.

## Rotating keys

1. Generate a new key pair for one side (say A).
2. Store A's new private key in A's Secret and A's new public key as `peer-public-key` in B's.
3. Restart both gateways: `kubectl -n zweik8s rollout restart deploy/zweik8s`.
4. Check `zweik8s status` on both sides.

Keys are read at start, so the tunnel is down between the two restarts. Rotate one side at a
time.

## Troubleshooting

| Symptom | Look at |
|---|---|
| `peerReplyAgeSeconds` stays `null`, `handshakeAgeSeconds` -1 | The peer cannot reach this side's UDP port, or the reverse. Check both Services' external addresses against both `peer.endpoint`s, firewalls in between, and that the keys pair up (A's `peer-public-key` is B's public key). |
| Handshakes happen, probes fail | The tunnel addresses do not match (`tunnel.address` on one side must equal `peer.address` on the other), or a different MTU in between drops large packets; lower `tunnel.mtu`. |
| The gateway does not start: `Operation not permitted` | The namespace does not allow privileged pods, or a policy engine strips `NET_ADMIN`. |
| Callers time out connecting to an import | The NetworkPolicy does not match the caller's labels or namespace. |
| An import connects, then closes at once | The export's target does not resolve or refuses; see `failed` and the exporting gateway's log. |
| `transportResets` keeps growing | The UDP path between the clusters is lossy or a NAT drops the flow; check `wireguardKeepaliveSeconds` and the network in between. |
| Throughput lower than the link | `zweik8s_bridge_errors_total` (socket buffers), CPU of the gateway pod, and whether BBR is available (a warning is logged once if not). |
