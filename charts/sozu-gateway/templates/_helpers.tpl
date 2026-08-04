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
The exposure entry serving a given Gateway API protocol, as JSON. Used where a
template needs one specific listener (Sōzu's static HTTP/HTTPS binds).
*/}}
{{- define "sozu-gateway.exposureFor" -}}
{{- $proto := .proto -}}
{{- range .root.Values.exposure -}}
{{- if eq .protocol $proto -}}{{ toJson . }}{{- end -}}
{{- end -}}
{{- end }}
