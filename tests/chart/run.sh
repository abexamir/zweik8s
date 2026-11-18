#!/usr/bin/env bash
# Render the chart with the examples and check what it produces and what it refuses.
# Usage: tests/chart/run.sh [path/to/zweik8s]   (with a binary, rendered configs are validated)
set -euo pipefail
cd "$(dirname "$0")/../.."
CHART=charts/zweik8s
BIN=${1:-}
OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT
PASS=0 FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok   $*"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }

helm lint --strict "$CHART" -f examples/site-a.yaml >/dev/null && ok "lint (site a)" || bad "lint (site a)"
helm lint --strict "$CHART" -f examples/site-b.yaml >/dev/null && ok "lint (site b)" || bad "lint (site b)"

for site in a b; do
  helm template zweik8s "$CHART" -n zweik8s -f "examples/site-$site.yaml" > "$OUT/$site.yaml"
  python3 - "$OUT/$site.yaml" "$OUT/$site.json" <<'PY'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
cm = next(d for d in docs if d["kind"] == "ConfigMap")
open(sys.argv[2], "w").write(cm["data"]["runtime.json"])
PY
  [ -n "$BIN" ] && { "$BIN" check "$OUT/$site.json" >/dev/null && ok "site $site config accepted by the runtime" || bad "site $site config rejected by the runtime"; }
done

expect() { # description, python expression over `docs` (site b)
  if python3 - "$OUT/b.yaml" "$2" <<'PY'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
kind = lambda k: [d for d in docs if d["kind"] == k]
cfg = json.loads(kind("ConfigMap")[0]["data"]["runtime.json"])
sys.exit(0 if eval(sys.argv[2]) else 1)
PY
  then ok "$1"; else bad "$1"; fi
}
expect "import Service uses serviceName and targets the tunnel port" \
  'any(s["metadata"]["name"] == "postgres-site-a" and s["spec"]["ports"][0]["targetPort"] == 20001 for s in kind("Service"))'
expect "export listener binds the tunnel and targets the host" \
  'any(l["bind"] == "tunnel" and l["targetHost"] == "redis-master.cache.svc.cluster.local" for l in cfg["listeners"])'
expect "import listener targets the peer tunnel address" \
  'any(l["bind"] == "pod" and l["targetHost"] == "169.254.100.1" and l["listenPort"] == 20001 for l in cfg["listeners"])'
expect "NetworkPolicy admits each caller and the scraper" \
  'len(kind("NetworkPolicy")[0]["spec"]["ingress"]) == 3 and len(kind("NetworkPolicy")[0]["spec"]["ingress"][1]["from"]) == 2'
expect "wildcard caller has an empty namespaceSelector" \
  'kind("NetworkPolicy")[0]["spec"]["ingress"][1]["from"][1]["namespaceSelector"] == {}'
expect "metrics bind all addresses and a PodMonitor exists" \
  'cfg["health"]["bind"] == "0.0.0.0:9090" and len(kind("PodMonitor")) == 1'
expect "pod: NET_ADMIN only, read-only root, no token" \
  'kind("Deployment")[0]["spec"]["template"]["spec"]["containers"][0]["securityContext"]["capabilities"] == {"drop": ["ALL"], "add": ["NET_ADMIN"]} and kind("Deployment")[0]["spec"]["template"]["spec"]["automountServiceAccountToken"] is False'
expect "config change rolls the pod" \
  'len(kind("Deployment")[0]["spec"]["template"]["metadata"]["annotations"]["zweik8s.io/config"]) == 64'
expect "LoadBalancer pins the nodePort" \
  'any(s["spec"].get("type") == "LoadBalancer" and s["spec"]["ports"][0]["nodePort"] == 31822 for s in kind("Service"))'

refuse() { # description, --set arguments...
  local what=$1; shift
  if helm template x "$CHART" -f examples/site-a.yaml "$@" >/dev/null 2>&1; then bad "refuses: $what"; else ok "refuses: $what"; fi
}
refuse "missing tunnel address" --set tunnel.address=
refuse "missing peer endpoint" --set peer.endpoint=
refuse "no key source" --set keys.existingSecret=
refuse "same tunnel addresses" --set peer.address=169.254.100.1
refuse "tunnelPort used twice" --set 'imports[0].ports[0].tunnelPort=20001'
refuse "import without callers" --set 'imports[0].callers=null'
refuse "caller without labels" --set 'imports[0].callers[0].matchLabels=null'
refuse "export without a target" --set 'exports[0].service=null'
refuse "bad protocol" --set 'exports[0].ports[0].protocol=SCTP'
refuse "bad tunnel address" --set tunnel.address=not-an-ip

echo "passed $PASS, failed $FAIL"
[ "$FAIL" = 0 ]
