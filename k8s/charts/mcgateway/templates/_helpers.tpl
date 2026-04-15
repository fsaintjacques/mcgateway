{{- define "mcgateway.name" -}}
mcgateway
{{- end -}}

{{- define "mcgateway.labels" -}}
app.kubernetes.io/name: {{ include "mcgateway.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}
