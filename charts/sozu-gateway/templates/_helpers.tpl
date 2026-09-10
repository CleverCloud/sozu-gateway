{{- define "sozu-gateway.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "sozu-gateway.baseFullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "sozu-gateway.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "sozu-gateway.fullname" -}}
{{- if .gatewayInstance -}}
{{- printf "%s-%s" (include "sozu-gateway.baseFullname" .) .gatewayInstance.name -}}
{{- else -}}
{{- include "sozu-gateway.baseFullname" . -}}
{{- end -}}
{{- end -}}

{{- define "sozu-gateway.instanceLabel" -}}
{{- if .gatewayInstance -}}
{{- printf "%s-%s" .Release.Name .gatewayInstance.name -}}
{{- else -}}
{{- .Release.Name -}}
{{- end -}}
{{- end -}}

{{- define "sozu-gateway.labels" -}}
app.kubernetes.io/name: {{ include "sozu-gateway.name" . }}
app.kubernetes.io/instance: {{ include "sozu-gateway.instanceLabel" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: sozu-gateway
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "sozu-gateway.selectorLabels" -}}
app.kubernetes.io/name: {{ include "sozu-gateway.name" . }}
app.kubernetes.io/instance: {{ include "sozu-gateway.instanceLabel" . }}
{{- end -}}

{{- define "sozu-gateway.serviceAccountName" -}}
{{ include "sozu-gateway.baseFullname" . }}
{{- end -}}

{{/* Old releases reused with --reuse-values do not contain this setting. */}}
{{- define "sozu-gateway.httpErrorPort" -}}
{{- if hasKey .Values.controller "httpErrorPort" -}}
{{- .Values.controller.httpErrorPort -}}
{{- else -}}8082{{- end -}}
{{- end -}}

{{- define "sozu-gateway.httpUnavailablePort" -}}
{{- if hasKey .Values.controller "httpUnavailablePort" -}}
{{- .Values.controller.httpUnavailablePort -}}
{{- else -}}8083{{- end -}}
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
{{- /* The Pod's other listeners are seeded into the bind map, so an exposure
     entry cannot quietly land on one. They are bound by the controller at
     startup, before the first reconcile: Sōzu's ActivateListener then fails
     EADDRINUSE in the first tier, which fails every reconcile including HTTP,
     and readiness never turns green. */ -}}
{{- $binds := dict -}}
{{- $errorPort := include "sozu-gateway.httpErrorPort" . -}}
{{- if or (not (regexMatch "^[0-9]+$" (toString $errorPort))) (lt (int $errorPort) 1025) (gt (int $errorPort) 65535) -}}
  {{- fail "controller.httpErrorPort must be an integer between 1025 and 65535" -}}
{{- end -}}
{{- if or (eq (int $errorPort) (int (.Values.controller.healthPort | default 8081))) (and .Values.metrics.enabled (eq (int $errorPort) (int .Values.metrics.port))) -}}
  {{- fail "controller.httpErrorPort must differ from the health and metrics ports" -}}
{{- end -}}
{{- $_ := set $binds (printf "%s/TCP" (toString $errorPort)) "the HTTP error backend" -}}
{{- $unavailablePort := include "sozu-gateway.httpUnavailablePort" . -}}
{{- if or (not (regexMatch "^[0-9]+$" $unavailablePort)) (lt (int $unavailablePort) 1025) (gt (int $unavailablePort) 65535) -}}
  {{- fail "controller.httpUnavailablePort must be an integer between 1025 and 65535" -}}
{{- end -}}
{{- if or (eq (int $unavailablePort) (int $errorPort)) (eq (int $unavailablePort) (int (.Values.controller.healthPort | default 8081))) (and .Values.metrics.enabled (eq (int $unavailablePort) (int .Values.metrics.port))) -}}
  {{- fail "controller.httpUnavailablePort must differ from the HTTP error, health and metrics ports" -}}
{{- end -}}
{{- $_ := set $binds (printf "%s/TCP" $unavailablePort) "the HTTP unavailable backend" -}}
{{- if .Values.metrics.enabled -}}
  {{- $_ := set $binds (printf "%s/TCP" (toString .Values.metrics.port)) "the metrics endpoint" -}}
{{- end -}}
{{- $_ := set $binds (printf "%s/TCP" (toString .Values.controller.healthPort)) "the controller's health endpoint" -}}
{{- /* Reserved names: the metrics Service resolves `targetPort` by name, and a
     name is looked up across every container in Pod order — Sōzu first. An
     exposure entry called `metrics` would silently point the endpoint at a Sōzu
     listener. */ -}}
{{- $reserved := list "metrics" "health" -}}
{{- $ports := dict -}}
{{- $l7 := dict "HTTP" 0 "HTTPS" 0 -}}
{{- range $i, $e := .Values.exposure -}}
  {{- if not $e -}}
    {{- fail (printf "exposure[%d] is empty: `--set exposure[N].x` replaces the whole list, so every entry has to be re-stated (use --set-json or a values file)" $i) -}}
  {{- end -}}
  {{- if or (not $e.name) (not $e.port) (not $e.bind) (not $e.protocol) -}}
    {{- fail (printf "exposure[%d] needs name, port, bind and protocol; got %s" $i (toJson $e)) -}}
  {{- end -}}
  {{- if has $e.name $reserved -}}
    {{- fail (printf "exposure entry %d is named %q, which the Pod already uses for a container port — the metrics Service resolves its target by name across every container, Sōzu first, so this would point it at a Sōzu listener. Pick another name" $i $e.name) -}}
  {{- end -}}
  {{- $transport := $e.transport | default "TCP" -}}
  {{- if lt (int $e.bind) 1025 -}}
    {{- fail (printf "exposure entry %q binds %d: nothing in the Pod can bind a privileged port — both containers run as uid %v with every capability dropped. Advertise the low port and bind a high one (the defaults map 80 -> 8080 and 443 -> 8443)" $e.name (int $e.bind) $.Values.runAsUser) -}}
  {{- end -}}
  {{- $bindKey := printf "%s/%s" (toString $e.bind) $transport -}}
  {{- if hasKey $binds $bindKey -}}
    {{- fail (printf "exposure entry %q binds %d/%s, already taken by %s — one socket cannot serve two" $e.name (int $e.bind) $transport (get $binds $bindKey)) -}}
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
  {{- if lt (int $count) 1 -}}
    {{- fail (printf "exposure must hold at least one %s entry: the first HTTP and HTTPS entries serve Ingress routes; additional entries serve Gateway API listeners" $proto) -}}
  {{- end -}}
{{- end -}}
{{- end -}}

{{/*
The first exposure entry for a protocol, as JSON. Ingress and health probes
use the first listener; Gateway API listeners select their advertised port.
*/}}
{{- define "sozu-gateway.exposureFor" -}}
{{- $proto := .proto -}}
{{- $first := dict -}}
{{- range .root.Values.exposure -}}
{{- if and (eq .protocol $proto) (not $first) -}}
{{- $_ := set $first "entry" . -}}
{{- end -}}
{{- end -}}
{{- with $first.entry -}}{{ toJson . }}{{- end -}}
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

{{/*
The drain's effective settings, defaulted here rather than read straight from
values: `helm upgrade --reuse-values` replays the previous release's values over
the new chart and does not pick up keys the chart has since added, so an existing
user upgrading into this feature arrives with `sozu.drain` absent. Reading it
blind is a nil dereference and a Go template trace; defaulting makes that upgrade
simply work.
*/}}
{{- define "sozu-gateway.drainDelay" -}}
{{- (.Values.sozu.drain | default dict).delaySeconds | default 5 -}}
{{- end -}}

{{- define "sozu-gateway.drainGrace" -}}
{{- (.Values.sozu.drain | default dict).gracePeriodSeconds | default 40 -}}
{{- end -}}

{{/*
Whether the hook is rendered at all. `default true` would be wrong here — it
treats `false` as unset — so the key's presence is what decides.
*/}}
{{- define "sozu-gateway.drainEnabled" -}}
{{- $drain := .Values.sozu.drain | default dict -}}
{{- if hasKey $drain "enabled" -}}{{ $drain.enabled }}{{- else -}}true{{- end -}}
{{- end -}}

{{/*
The drain has to fit inside the grace period, or the kubelet SIGKILLs mid-drain
and the hook has bought nothing.

Validated on the raw values, not on their coercion: `int` turns "30s" into 0 and
5.7 into 5, so validating the coerced form both hides the mistake and reports a
number the user never wrote.
*/}}
{{- define "sozu-gateway.validateDrain" -}}
{{- $drain := .Values.sozu.drain | default dict -}}
{{- range $key := list "delaySeconds" "gracePeriodSeconds" -}}
  {{- $value := get $drain $key -}}
  {{- /* Absent is fine — the defaults above cover it, which is what makes
       `--reuse-values` work. `get` on a missing key yields "", not nil, so the
       test is on the key's presence. */ -}}
  {{- if and (hasKey $drain $key) (not (kindIs "invalid" $value)) -}}
    {{- if not (regexMatch "^[0-9]+$" (printf "%v" $value)) -}}
      {{- fail (printf "sozu.drain.%s must be a whole number of seconds, got %v — it is rendered into a shell command and into terminationGracePeriodSeconds, neither of which takes a fraction or a unit suffix" $key $value) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- $d := int (include "sozu-gateway.drainDelay" .) -}}
{{- $g := int (include "sozu-gateway.drainGrace" .) -}}
{{- if le $g $d -}}
  {{- fail (printf "sozu.drain.gracePeriodSeconds (%d) must exceed sozu.drain.delaySeconds (%d), otherwise the kubelet SIGKILLs the proxy before it has begun draining" $g $d) -}}
{{- end -}}
{{- end -}}

{{/* An explicit list provisions independent Pods and addresses. Reject names
     instead of truncating them into another instance's selectors or resources. */}}
{{- define "sozu-gateway.validateGatewayInstances" -}}
{{- $names := dict -}}
{{- $gateways := dict -}}
{{- $base := include "sozu-gateway.baseFullname" . -}}
{{- $services := dict $base "default" (printf "%s-metrics" $base) "default metrics" -}}
{{- $instances := .Values.gatewayInstances | default list -}}
{{- if not (kindIs "slice" $instances) -}}
  {{- fail "gatewayInstances must be a list" -}}
{{- end -}}
{{- $label := "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$" -}}
{{- range $i, $instance := $instances -}}
  {{- if not (kindIs "map" $instance) -}}
    {{- fail (printf "gatewayInstances[%d] must be a mapping" $i) -}}
  {{- end -}}
  {{- $name := required (printf "gatewayInstances[%d].name is required" $i) $instance.name -}}
  {{- if or (not (kindIs "string" $name)) (not (regexMatch $label $name)) (gt (len $name) 63) -}}
    {{- fail (printf "gatewayInstances[%d].name must be a DNS label" $i) -}}
  {{- end -}}
  {{- if hasKey $names $name -}}{{- fail (printf "gatewayInstances repeats instance name %q" $name) -}}{{- end -}}
  {{- $_ := set $names $name true -}}
  {{- $full := printf "%s-%s" (include "sozu-gateway.baseFullname" $) $name -}}
  {{- if or (gt (len $full) 55) (gt (len (printf "%s-%s" $.Release.Name $name)) 63) -}}
    {{- fail (printf "gatewayInstances name %q is too long with this release: use a shorter name or fullnameOverride (the -metrics suffix must fit 63 characters)" $name) -}}
  {{- end -}}
  {{- range $serviceName := list $full (printf "%s-metrics" $full) -}}
    {{- if hasKey $services $serviceName -}}
      {{- fail (printf "gatewayInstances name %q collides with Service %s (%s)" $name $serviceName (get $services $serviceName)) -}}
    {{- end -}}
    {{- $_ := set $services $serviceName $name -}}
  {{- end -}}
  {{- $gateway := required (printf "gatewayInstances[%d].gateway is required" $i) $instance.gateway -}}
  {{- if not (kindIs "map" $gateway) -}}{{- fail "gatewayInstances[].gateway must be a mapping" -}}{{- end -}}
  {{- $ns := required "gatewayInstances[].gateway.namespace is required" $gateway.namespace -}}
  {{- $gwName := required "gatewayInstances[].gateway.name is required" $gateway.name -}}
  {{- if or (not (kindIs "string" $ns)) (not (regexMatch $label $ns)) (gt (len $ns) 63) -}}
    {{- fail "gatewayInstances[].gateway.namespace must be a DNS label" -}}
  {{- end -}}
  {{- if or (not (kindIs "string" $gwName)) (gt (len $gwName) 253) -}}{{- fail "gatewayInstances[].gateway.name must be a DNS subdomain" -}}{{- end -}}
  {{- range $part := splitList "." $gwName -}}
    {{- if or (not (regexMatch $label $part)) (gt (len $part) 63) -}}{{- fail "gatewayInstances[].gateway.name must be a DNS subdomain" -}}{{- end -}}
  {{- end -}}
  {{- $key := printf "%s/%s" $ns $gwName -}}
  {{- if hasKey $gateways $key -}}{{- fail (printf "gatewayInstances assigns Gateway %s more than once" $key) -}}{{- end -}}
  {{- $_ := set $gateways $key true -}}
  {{- if and (hasKey $instance "service") (not (kindIs "map" $instance.service)) -}}{{- fail "gatewayInstances[].service must be a mapping" -}}{{- end -}}
  {{- if hasKey $instance "replicaCount" -}}
    {{- if or (not (regexMatch "^[1-9][0-9]*$" (printf "%v" $instance.replicaCount))) (gt (int $instance.replicaCount) 2147483647) -}}
      {{- fail "gatewayInstances[].replicaCount must be a positive integer" -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- end -}}

{{/* Render one resource template for the default instance, then each explicit
     Gateway instance. Only Service settings and replica count may differ;
     all use the release's exposure, image and shared ServiceAccount. */}}
{{- define "sozu-gateway.renderInstances" -}}
{{- $root := .root -}}
{{- $template := .template -}}
{{- include "sozu-gateway.validateGatewayInstances" $root -}}
{{- include $template $root -}}
{{- range $instance := $root.Values.gatewayInstances | default list -}}
  {{- $values := deepCopy $root.Values -}}
  {{- if $instance.service -}}
    {{- $_ := set $values "service" (mergeOverwrite (deepCopy $root.Values.service) $instance.service) -}}
  {{- end -}}
  {{- if hasKey $instance "replicaCount" -}}{{- $_ := set $values "replicaCount" $instance.replicaCount -}}{{- end -}}
  {{- $context := merge (dict "gatewayInstance" $instance "Values" $values) $root -}}
  {{- $rendered := include $template $context -}}
  {{- if trim $rendered -}}{{ printf "\n---\n%s" $rendered }}{{- end -}}
{{- end -}}
{{- end -}}
