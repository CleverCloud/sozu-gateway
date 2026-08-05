{{- define "sozu-gateway.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "sozu-gateway.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "sozu-gateway.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "sozu-gateway.labels" -}}
app.kubernetes.io/name: {{ include "sozu-gateway.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: sozu-gateway
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "sozu-gateway.selectorLabels" -}}
app.kubernetes.io/name: {{ include "sozu-gateway.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "sozu-gateway.serviceAccountName" -}}
{{ include "sozu-gateway.fullname" . }}
{{- end -}}

{{/*
Reject an exposure table that cannot work, with a message that says why.

Every failure below otherwise surfaces as something far less legible: a raw
apiserver validation error on the Service, a Sōzu listener that never binds, or
a reconcile that fails as a whole. In particular a layer-4 entry on 443 is not
"unsupported", it is impossible — the Service already publishes 443/TCP for
`https`, and a Service cannot expose one (port, protocol) twice — so it is
caught here rather than left to `helm install` to reject obscurely.
*/}}
{{- define "sozu-gateway.validateExposure" -}}
{{- $binds := dict -}}
{{- $ports := dict -}}
{{- $l7 := dict "HTTP" 0 "HTTPS" 0 -}}
{{- range $i, $e := .Values.exposure -}}
  {{- if not $e -}}
    {{- fail (printf "exposure[%d] is empty: `--set exposure[N].x` replaces the whole list, so every entry has to be re-stated (use --set-json or a values file)" $i) -}}
  {{- end -}}
  {{- if or (not $e.name) (not $e.port) (not $e.bind) (not $e.protocol) -}}
    {{- fail (printf "exposure[%d] needs name, port, bind and protocol; got %s" $i (toJson $e)) -}}
  {{- end -}}
  {{- $transport := $e.transport | default "TCP" -}}
  {{- if lt (int $e.bind) 1025 -}}
    {{- fail (printf "exposure entry %q binds %d: nothing in the Pod can bind a privileged port — both containers run as uid %v with every capability dropped. Advertise the low port and bind a high one (the defaults map 80 -> 8080 and 443 -> 8443)" $e.name (int $e.bind) $.Values.runAsUser) -}}
  {{- end -}}
  {{- $bindKey := printf "%s/%s" (toString $e.bind) $transport -}}
  {{- if hasKey $binds $bindKey -}}
    {{- fail (printf "exposure entries %q and %q both bind %d/%s — one socket cannot serve two" (get $binds $bindKey) $e.name (int $e.bind) $transport) -}}
  {{- end -}}
  {{- $_ := set $binds $bindKey $e.name -}}
  {{- $portKey := printf "%s/%s" (toString $e.port) $transport -}}
  {{- if hasKey $ports $portKey -}}
    {{- fail (printf "exposure entries %q and %q both advertise %d/%s — a Service cannot expose one (port, protocol) twice. Layer-4 routing on 443 is impossible for exactly this reason: `https` already holds it" (get $ports $portKey) $e.name (int $e.port) $transport) -}}
  {{- end -}}
  {{- $_ := set $ports $portKey $e.name -}}
  {{- if hasKey $l7 $e.protocol -}}
    {{- $_ := set $l7 $e.protocol (add1 (get $l7 $e.protocol)) -}}
  {{- end -}}
{{- end -}}
{{- range $proto, $count := $l7 -}}
  {{- if ne (int $count) 1 -}}
    {{- fail (printf "exposure must hold exactly one %s entry, found %d: Sōzu's HTTP and HTTPS listeners are declared in config.toml and bound at boot, one per protocol — re-creating one would drop its certificate store, so there is nowhere for a second to come from. Layer-4 (TCP/UDP) entries have no such limit" $proto (int $count)) -}}
  {{- end -}}
{{- end -}}
{{- range $port, $svc := .Values.l4.tcpServices -}}
  {{- if not (hasKey $ports (printf "%s/TCP" (toString $port))) -}}
    {{- fail (printf "l4.tcpServices maps port %v to %q, but no exposure entry advertises %v on TCP — the Service would have no port routing there, so the controller reports the mapping (L4PortNotExposed) and programs nothing. Add: {name: tcp-%v, port: %v, bind: %v, protocol: TCP, transport: TCP}" $port $svc $port $port $port $port) -}}
  {{- end -}}
{{- end -}}
{{- range $port, $svc := .Values.l4.udpServices -}}
  {{- if not (hasKey $ports (printf "%s/UDP" (toString $port))) -}}
    {{- fail (printf "l4.udpServices maps port %v to %q, but no exposure entry advertises %v on UDP — the Service would have no port routing there, so the controller reports the mapping (L4PortNotExposed) and programs nothing. Add: {name: udp-%v, port: %v, bind: %v, protocol: UDP, transport: UDP}" $port $svc $port $port $port $port) -}}
  {{- end -}}
{{- end -}}
{{- end -}}

{{/*
The exposure entry serving a given Gateway API protocol, as JSON. Used where a
template needs one specific listener (Sōzu's static HTTP/HTTPS binds).
*/}}
{{- define "sozu-gateway.exposureFor" -}}
{{- $proto := .proto -}}
{{- range .root.Values.exposure -}}
{{- if eq .protocol $proto -}}{{ toJson . }}{{- end -}}
{{- end -}}
{{- end }}
