# Configuration

One Helm release per cluster. Values not listed here are standard (`resources`, `nodeSelector`,
`tolerations`, `podAnnotations`, `podLabels`, `priorityClassName`, `imagePullSecrets`).
`values.yaml` documents every key; `values.schema.json` and the templates reject mistakes at
render time.

## What both sides must agree on

| This side | Must equal on the other side |
|---|---|
| `tunnel.address` | `peer.address` |
| the Service's external address and port | `peer.endpoint` |
| `exports[].ports[].tunnelPort` | `imports[].ports[].tunnelPort` |
| the public key of `private-key` | `peer-public-key` |

A `tunnelPort` is unique per protocol across both sides: it identifies the export.

## Tunnel and peer

```yaml
tunnel:
  address: 169.254.100.1   # this side's /32 inside the tunnel
  listenPort: 51822        # UDP port in the pod; WireGuard itself uses 51823 privately
  mtu: 1380                # leave room for WireGuard (60 bytes) and your network's own overhead
peer:
  address: 169.254.100.2
  endpoint: gw.site-b.example.com:31822
```

Pick tunnel addresses no workload uses. Link-local `169.254.x.y` pairs are a good choice.
Lower the MTU if the path between the clusters has its own encapsulation (e.g. 1280 over an
IPsec VPN).

## Exposing the encrypted port

The peer must reach this side's UDP `listenPort`. Pick one:

```yaml
service:                       # a cloud or MetalLB LoadBalancer
  type: LoadBalancer
  nodePort: 31821              # pin it: the peer's endpoint must not change
  loadBalancerClass: ""        # e.g. a specific LB implementation
  annotations: {}
  externalTrafficPolicy: Local # keep the client address; send only to the gateway's node
```

```yaml
service:                       # a NodePort behind your own router/NAT
  type: NodePort
  nodePort: 31821
```

With `externalTrafficPolicy: Local`, only the node running the gateway answers. Pin the gateway
to nodes your NAT forwards to with `preferredNodes` or `nodeSelector`.

## Keys

The Secret holds `private-key` (this side's WireGuard private key) and `peer-public-key`.

```yaml
keys:
  existingSecret: zweik8s-keys
```

Or with [External Secrets Operator](https://external-secrets.io), e.g. from Vault:

```yaml
keys:
  externalSecret:
    enabled: true
    secretStoreRef: {name: vault, kind: ClusterSecretStore}
    remoteKey: network/zweik8s/site-a        # PRIVATE_KEY and PEER_PUBKEY properties
```

Keys are read at start. After rotating, restart the gateways (see
[operations](operations.md#rotating-keys)).

## Exports

```yaml
exports:
  - name: postgres
    service: postgres          # service + namespace, resolved as <service>.<namespace>.svc.<clusterDomain>
    namespace: databases
    ports:
      - {port: 5432, tunnelPort: 20001}             # protocol defaults to TCP
  - name: dns
    host: kube-dns.kube-system.svc.cluster.local    # or any host name or IP this pod can reach
    ports:
      - {port: 53, tunnelPort: 20053, protocol: UDP}
```

The name is resolved for every new TCP connection, so Service and endpoint changes apply
immediately. Set `clusterDomain` if yours is not `cluster.local`.

## Imports

```yaml
imports:
  - name: postgres
    serviceName: postgres-site-b   # defaults to name
    ports:
      - {port: 5432, tunnelPort: 20001}
    callers:
      - namespace: billing          # pods in namespace billing with app=invoicer
        matchLabels: {app: invoicer}
      - namespace: "*"              # pods in any namespace with this label
        matchLabels: {zweik8s.io/access: "true"}
```

`callers` become NetworkPolicy rules on the gateway, so a CNI that enforces NetworkPolicy is
required for caller isolation. `matchLabels` must not be empty: "every pod" is never implied.

Callers reach the import as `<serviceName>.<release namespace>.svc.cluster.local`. Keep TLS
verification on and use the target's real name, e.g. `sslmode=verify-full` with libpq `host`
set to the certificate name and `hostaddr` to the import's address, or Vault's
`VAULT_TLS_SERVER_NAME`.

## Metrics

```yaml
metrics:
  enabled: true                 # serve /metrics on the pod IP, port 9090
  allowFrom:                    # NetworkPolicy: who may scrape
    - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: monitoring}}
  podMonitor: {enabled: true}   # Prometheus Operator
  vmPodScrape: {enabled: false} # VictoriaMetrics Operator
```

## Tuning

`tuning` passes through to the runtime. Defaults suit most links.

| Key | Default | Meaning |
|---|---|---|
| `maxSessions` | 1024 | Concurrent sessions per listener; more are refused and counted |
| `connectTimeoutSeconds` | 10 | DNS plus connect to a target |
| `tcpCongestion` | `bbr` | Congestion control on tunnel legs; `""` keeps the kernel default. Falls back silently if unavailable |
| `splice` | true | Zero-copy TCP relay; false copies through user space |
| `socketBufferBytes` | 4194304 | Buffers of the encrypted UDP sockets |
| `keepaliveIdleSeconds` / `IntervalSeconds` / `Count` | 60 / 10 / 3 | TCP keepalive on both legs: dead peers are detected, idle healthy sessions are kept |
| `udpIdleSeconds` | 60 | Idle UDP associations expire |
| `udpRefreshSeconds` | 15 | UDP associations are re-created, re-resolving the target |
| `probeIntervalSeconds` | 5 | Tunnel probe interval |
| `peerTimeoutSeconds` | 45 | Silence before the transport is reset |
| `wireguardKeepaliveSeconds` | 15 | WireGuard persistent keepalive, keeps NAT mappings open |
| `drainSeconds` | 10 | Wait for open sessions on SIGTERM |
| `workers` | 0 | Runtime threads; 0 means one per usable CPU |

## The runtime config

The chart renders `runtime.json` into a ConfigMap. Outside Kubernetes, run the binary with
your own file (`zweik8s run --config file.json`); `zweik8s check file.json` validates one.
Listeners there are explicit:

```json
{"localAddress": "169.254.100.1", "peerAddress": "169.254.100.2",
 "peerEndpoint": "198.51.100.20:31822", "listenPort": 51822,
 "keys": {"privateKey": "/keys/private-key", "peerPublicKey": "/keys/peer-public-key"},
 "listeners": [
   {"name": "postgres", "bind": "pod", "protocol": "TCP", "listenPort": 20001,
    "targetHost": "169.254.100.2", "targetPort": 20001}]}
```

`bind: pod` (an import) listens on `$POD_IP` and must target the peer's tunnel address.
`bind: tunnel` (an export) listens on the local tunnel address.
