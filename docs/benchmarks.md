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

Two production clusters about 5 ms apart, the public internet in between, single-stream HTTPS
between them (no tunnel) measured at 150–320 Mbit/s. The Python prototype delivered 75–100
Mbit/s through the tunnel and used a full core on the gateway. zweik8s numbers between the same
clusters: see the release notes of 0.1.0.

## Reproduce

```sh
cargo build --release
sudo BENCH=1 tests/e2e/run.sh target/release/zweik8s
```

On a cluster: run an iperf3 server behind an export and a client against the import, and read
`zweik8s_encrypted_bytes_total` and the gateway's CPU while it runs.
