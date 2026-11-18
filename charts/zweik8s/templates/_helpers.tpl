{{- define "zweik8s.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "zweik8s.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "zweik8s.labels" -}}
{{ include "zweik8s.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "zweik8s.secretName" -}}
{{- .Values.keys.existingSecret | default (include "zweik8s.fullname" .) -}}
{{- end -}}

{{- define "zweik8s.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- .Values.serviceAccount.name | default (include "zweik8s.fullname" .) -}}
{{- else -}}
{{- .Values.serviceAccount.name | default "default" -}}
{{- end -}}
{{- end -}}

{{- define "zweik8s.image" -}}
{{- $i := .Values.image -}}
{{- if $i.digest -}}
{{- printf "%s@%s" $i.repository $i.digest -}}
{{- else -}}
{{- printf "%s:%s" $i.repository ($i.tag | default .Chart.AppVersion) -}}
{{- end -}}
{{- end -}}

{{/* Validate values and build the runtime config. Fails the render on any mistake. */}}
{{- define "zweik8s.runtime" -}}
{{- $v := .Values -}}
{{- if not $v.tunnel.address }}{{ fail "tunnel.address is required" }}{{ end -}}
{{- if not $v.peer.address }}{{ fail "peer.address is required" }}{{ end -}}
{{- if not $v.peer.endpoint }}{{ fail "peer.endpoint is required (host:port)" }}{{ end -}}
{{- if eq $v.tunnel.address $v.peer.address }}{{ fail "tunnel.address and peer.address must differ" }}{{ end -}}
{{- if and (not $v.keys.existingSecret) (not $v.keys.externalSecret.enabled) -}}
{{- fail "set keys.existingSecret or enable keys.externalSecret" -}}
{{- end -}}
{{- $used := dict -}}
{{- $names := dict -}}
{{- $listeners := list -}}
{{- range $e := $v.exports -}}
{{- if not $e.name }}{{ fail "every export needs a name" }}{{ end -}}
{{- if hasKey $names (printf "export/%s" $e.name) }}{{ fail (printf "duplicate export %s" $e.name) }}{{ end -}}
{{- $_ := set $names (printf "export/%s" $e.name) true -}}
{{- $host := $e.host -}}
{{- if not $host -}}
{{- if not (and $e.service $e.namespace) }}{{ fail (printf "export %s: set host, or service and namespace" $e.name) }}{{ end -}}
{{- $host = printf "%s.%s.svc.%s" $e.service $e.namespace $v.clusterDomain -}}
{{- end -}}
{{- if not $e.ports }}{{ fail (printf "export %s: ports are required" $e.name) }}{{ end -}}
{{- range $p := $e.ports -}}
{{- $proto := $p.protocol | default "TCP" | upper -}}
{{- $key := printf "%v/%s" $p.tunnelPort $proto -}}
{{- if hasKey $used $key }}{{ fail (printf "tunnelPort %s is used twice" $key) }}{{ end -}}
{{- $_ := set $used $key true -}}
{{- $listeners = append $listeners (dict "name" $e.name "bind" "tunnel" "protocol" $proto "listenPort" (int $p.tunnelPort) "targetHost" $host "targetPort" (int $p.port)) -}}
{{- end -}}
{{- end -}}
{{- range $i := $v.imports -}}
{{- if not $i.name }}{{ fail "every import needs a name" }}{{ end -}}
{{- if hasKey $names (printf "import/%s" $i.name) }}{{ fail (printf "duplicate import %s" $i.name) }}{{ end -}}
{{- $_ := set $names (printf "import/%s" $i.name) true -}}
{{- if not $i.ports }}{{ fail (printf "import %s: ports are required" $i.name) }}{{ end -}}
{{- if not $i.callers }}{{ fail (printf "import %s: callers are required" $i.name) }}{{ end -}}
{{- range $c := $i.callers -}}
{{- if not $c.matchLabels }}{{ fail (printf "import %s: every caller needs matchLabels" $i.name) }}{{ end -}}
{{- if not $c.namespace }}{{ fail (printf "import %s: every caller needs a namespace (or \"*\")" $i.name) }}{{ end -}}
{{- end -}}
{{- range $p := $i.ports -}}
{{- $proto := $p.protocol | default "TCP" | upper -}}
{{- $key := printf "%v/%s" $p.tunnelPort $proto -}}
{{- if hasKey $used $key }}{{ fail (printf "tunnelPort %s is used twice" $key) }}{{ end -}}
{{- $_ := set $used $key true -}}
{{- $listeners = append $listeners (dict "name" $i.name "bind" "pod" "protocol" $proto "listenPort" (int $p.tunnelPort) "targetHost" $v.peer.address "targetPort" (int $p.tunnelPort)) -}}
{{- end -}}
{{- end -}}
{{- $health := printf "127.0.0.1:%v" $v.metrics.port -}}
{{- if $v.metrics.enabled }}{{ $health = printf "0.0.0.0:%v" $v.metrics.port }}{{ end -}}
{{- $cfg := dict
  "interface" $v.tunnel.interface
  "localAddress" $v.tunnel.address
  "peerAddress" $v.peer.address
  "peerEndpoint" $v.peer.endpoint
  "listenPort" (int $v.tunnel.listenPort)
  "mtu" (int $v.tunnel.mtu)
  "controlPort" (int $v.tunnel.controlPort)
  "listeners" $listeners
  "tuning" ($v.tuning | default dict)
  "health" (dict "bind" $health) -}}
{{- toPrettyJson $cfg -}}
{{- end -}}
