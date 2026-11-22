# Benchmarks

## Same host, two network namespaces

Two gateways on one Ubuntu 24.04 laptop (12 threads, kernel 7.0), joined by a veth pair, each
gateway pinned to 2 CPUs with `taskset`. iperf3 and the echo client run on other CPUs. This
measures the gateways themselves, since the "network" is a veth with no limit. The baseline is
the Python/asyncio prototype that zweik8s replaced, in the same topology.

| | Python prototype | zweik8s 0.1 |
|---|---|---|
| TCP, 1 stream | 195 Mbit/s | **1.53 Gbit/s** |
| TCP, 1 stream, reverse | 174 Mbit/s | **1.54 Gbit/s** |
| TCP, 4 streams | 199 Mbit/s | **1.56 Gbit/s** |
| 64-byte request/response, p50 / p99 | 355 / 380 µs | **190 / 216 µs** |
| New TCP connections per second | 675 | **1,448** |

The prototype was bound to one core: its relay and its encrypted-UDP bridge shared one event
loop. zweik8s runs the bridge directions and every session on all available cores. With 2 CPUs
per gateway, the limit is CPU for WireGuard's own encryption plus the bridge.

## Between two data centers

Two production clusters about 5 ms apart, the public internet in between. iperf3 from a pod in
cluster B to a pod in cluster A, 12–15 s per run, three rounds interleaved with the same server
exposed directly through a LoadBalancer (no tunnel, CUBIC). Gateways: 0.1.1, cluster A on a
14-core node, cluster B on an 8-core node. Mbit/s, one value per round:

| | Direct (LoadBalancer) | zweik8s |
|---|---|---|
| B→A, 1 stream | 241, 316, 306 | **559, 574, 567** |
| A→B, 1 stream | 482, 505, 403 | 440, 459, 471 |
| B→A, 4 streams | 549, 484 | **609, 639, 624** |
| A→B, 4 streams | 496, 1175 | 457, 468 |

- Single streams over this lossy path do better through the tunnel: BBR on the tunnel legs
  recovers from loss that halves CUBIC on the direct path (direct runs retransmitted 10–100×
  more).
- All tunnel traffic is one UDP flow. In the A→B direction it held at about 450–470 Mbit/s
  while parallel direct flows sometimes reached 1.2 Gbit/s, which suggests a per-flow limit on
  that path rather than in the gateways (both stayed below one busy core).
- On a 2-CPU node the gateway in cluster B capped both directions near 500 Mbit/s; give it a
  node with spare cores.
- The Python prototype carried 75–100 Mbit/s between the same clusters.

## Reproduce

```sh
cargo build --release
sudo BENCH=1 tests/e2e/run.sh target/release/zweik8s
```

On a cluster: run an iperf3 server behind an export and a client against the import, and read
`zweik8s_encrypted_bytes_total` and the gateway's CPU while it runs.
