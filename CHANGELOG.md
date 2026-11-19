# Changelog

## 0.1.1

- Image builds for arm64 again: the Dockerfile checks that `wg`, `ip` and `nft` exist instead of
  running them, which failed under emulation. 0.1.0 was never published.

## 0.1.0

First release.

- Rust gateway: kernel WireGuard in the pod's own network namespace, TCP and UDP relays for
  imports and exports, and nftables admitting only declared exports from the tunnel.
- Multi-threaded data path: zero-copy TCP with splice(2), batched encrypted UDP with
  recvmmsg/sendmmsg, BBR on tunnel legs, 4 MiB UDP socket buffers.
- NAT-proof transport: encrypted traffic pinned to the configured peer endpoint (DNS names
  allowed); a probe through the tunnel resets the flow and the handshake after silence.
- `/metrics`, `/status`, `/readyz`, `/livez`, and `zweik8s status`; JSON logs.
- Graceful shutdown that drains open sessions.
- Helm chart with values validation, NetworkPolicy per import caller, ExternalSecret support,
  PodMonitor and VMPodScrape.
