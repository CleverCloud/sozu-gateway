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
{{- if or (.Values.l4).tcpServices (.Values.l4).udpServices -}}
  {{- fail "l4.tcpServices/l4.udpServices are removed — layer-4 routing is a TCPRoute or a UDPRoute now. Helm ignores unknown values, so this check exists to stop an upgrade from silently dropping your layer-4 routes. Migration: docs/UPGRADING.md" -}}
{{- end -}}
{{- range $proto, $count := $l7 -}}
  {{- if ne (int $count) 1 -}}
    {{- fail (printf "exposure must hold exactly one %s entry, found %d: Sōzu's HTTP and HTTPS listeners are declared in config.toml and bound at boot, one per protocol — re-creating one would drop its certificate store, so there is nowhere for a second to come from. Layer-4 (TCP/UDP) entries have no such limit" $proto (int $count)) -}}
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

{{/*
The timeout keys this chart exposes, in the order they are rendered. One list
feeds both the renderer and the validator, so the two cannot drift.

`sozu.timeouts.<name>` maps onto `<name>_timeout` in Sōzu's config. These four
are not everything `ListenerBuilder` understands — `sni_preread_timeout` and the
H2 deadlines exist too — they are the ones a gateway operator has a reason to
turn. Read that struct, not this list, for the full schema.
*/}}
{{- define "sozu-gateway.timeoutKeys" -}}
connect front back request
{{- end -}}

{{/*
Timeouts as top-level TOML keys. An empty entry is omitted, leaving Sōzu's own
default in force.

Emitted once at the root rather than inside each `[[listeners]]` block:
`assign_config_timeouts` applies a file-level timeout to every listener that
does not override it, so one copy covers both listeners and there is no second
copy to drift — nor any risk of a scalar landing after a sub-table like
`[listeners.hsts]` and being parsed into the wrong table.
*/}}
{{- define "sozu-gateway.timeouts" -}}
{{- $values := .Values.sozu.timeouts | default dict -}}
{{- range $key := splitList " " (include "sozu-gateway.timeoutKeys" $) -}}
{{- with get $values $key }}
{{ $key }}_timeout = {{ int . }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
Reject, while rendering, a timeout Sōzu could not use.

Two different failures are being headed off. A value the TOML cannot represent —
a fraction, or anything past `u32` — fails `FileConfig`'s whole-file parse, and
`sozu start` exits rather than serving: a CrashLoopBackOff, not a proxy missing a
listener. An unknown key is the quieter one: nothing at the file's top level
denies unknown fields, so a typo is dropped in silence and the operator's intent
simply never happens.
*/}}
{{- define "sozu-gateway.validateTimeouts" -}}
{{- $known := splitList " " (include "sozu-gateway.timeoutKeys" .) -}}
{{- $values := .Values.sozu.timeouts | default dict -}}
{{- if not (kindIs "map" $values) -}}
  {{- fail (printf "sozu.timeouts must be a mapping of %s, got %v" (join "/" $known) $values) -}}
{{- end -}}
{{- range $key, $value := $values -}}
  {{- if not (has $key $known) -}}
    {{- fail (printf "sozu.timeouts.%s is not a timeout this chart exposes — it renders %s. Nothing at the top level of Sōzu's config rejects an unknown key, so this would be dropped in silence rather than reported" $key (join ", " $known)) -}}
  {{- end -}}
  {{- if not (kindIs "invalid" $value) -}}
    {{- if or (kindIs "bool" $value) (kindIs "string" $value) (kindIs "map" $value) (kindIs "slice" $value) -}}
      {{- fail (printf "sozu.timeouts.%s must be a whole number of seconds, got %v (%s)" $key $value (kindOf $value)) -}}
    {{- end -}}
    {{- if ne (printf "%v" $value) (printf "%v" (int $value)) -}}
      {{- fail (printf "sozu.timeouts.%s must be a whole number of seconds, got %v — Sōzu's timeouts have one-second resolution and the fraction would be dropped without a word" $key $value) -}}
    {{- end -}}
    {{- if lt (int $value) 1 -}}
      {{- fail (printf "sozu.timeouts.%s must be at least 1 second, got %v — leave it empty to keep Sōzu's own default" $key $value) -}}
    {{- end -}}
    {{- if gt (int $value) 4294967295 -}}
      {{- fail (printf "sozu.timeouts.%s is %v, past the u32 Sōzu parses it into — the config would fail to load and the proxy would not start" $key $value) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- end -}}
