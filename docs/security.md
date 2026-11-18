# Security

## What is enforced, and where

| Threat | Control | Where |
|---|---|---|
| Someone on the internet sends packets to the public port | WireGuard drops anything not authenticated with the peer's key | kernel |
| A pod in this cluster uses an import it should not | NetworkPolicy admits only `imports[].callers` to each import port | your CNI |
| The peer (or its compromised gateway) reaches a Service you did not export | nftables in the gateway pod accepts from the tunnel only the declared export ports and the probe; everything else is dropped | pod netns |
| The peer routes raw packets through the gateway into your cluster | nftables drops all forwarded packets; WireGuard accepts only the peer's `/32` | pod netns |
| Traffic is read or altered between the clusters | WireGuard (Noise IK, ChaCha20-Poly1305) | kernel |
| A gateway's credentials leak | It has no Kubernetes API token (`automountServiceAccountToken: false`) and only its own WireGuard key | pod |

The peer can reach exactly your exports, and only through the gateway. Your pods can reach
exactly the imports their labels allow.

## The gateway pod

- `NET_ADMIN` only, in its own network namespace. It creates one interface and one nftables
  table there and touches nothing on the node.
- Root user, because `ip`, `wg` and `nft` need the capability effective. Everything else is
  dropped, the root filesystem is read-only, privilege escalation is off, seccomp is
  `RuntimeDefault`.
- The namespace must allow privileged pods (`pod-security.kubernetes.io/enforce=privileged`),
  which is why it should hold zweik8s alone: anything else there gets the same latitude.

## Things to keep in mind

- **NetworkPolicy is additive.** Another policy in the gateway's namespace that allows more
  ingress widens access to the imports. Keep the namespace dedicated, and watch for policies
  added to it.
- **Node and cluster admins are trusted.** They can read the key Secret, exec into the gateway,
  or run privileged pods.
- **The target sees the gateway, not the caller.** Source-IP allow-lists on the exporting side
  see the gateway pod's IP. Authenticate callers at the application layer (database users, mTLS,
  tokens).
- **TLS passes through untouched.** The import's name differs from the target's certificate.
  Keep verification on and connect with the real name (SNI / `verify-full`), never by turning
  verification off.
- **Keys.** The private key is mounted read-only with mode 0400 and never logged or passed as
  an argument. Prefer External Secrets from a vault to a hand-made Secret, and rotate
  periodically.
