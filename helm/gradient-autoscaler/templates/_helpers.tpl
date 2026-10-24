{{- define "gradient-autoscaler.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "gradient-autoscaler.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "gradient-autoscaler.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "gradient-autoscaler.workerNamespaces" -}}
{{- $namespaces := list -}}
{{- range $name, $connection := .Values.connections -}}
{{- if $connection.kubernetes -}}
{{- $namespaces = append $namespaces ($connection.kubernetes.namespace | default $.Release.Namespace) -}}
{{- end -}}
{{- end -}}
{{- $namespaces | uniq | toJson -}}
{{- end -}}

{{- define "gradient-autoscaler.config" -}}
{{- $connections := list -}}
{{- range $name, $connection := .Values.connections -}}
{{- if eq (and $connection.kubernetes $connection.githubActions | toString) "true" -}}
{{- fail (printf "connection %s: set exactly one of kubernetes and githubActions" $name) -}}
{{- end -}}
{{- $entry := omit $connection "apiKeySecret" "kubernetes" "githubActions" -}}
{{- $_ := set $entry "name" $name -}}
{{- $_ := set $entry "apiKeyFile" (printf "/run/secrets/gradient-autoscaler/%s/api-key" $name) -}}
{{- if $connection.kubernetes -}}
{{- $kubernetes := deepCopy $connection.kubernetes -}}
{{- $_ := set $kubernetes "namespace" ($connection.kubernetes.namespace | default $.Release.Namespace) -}}
{{- $_ := set $entry "kubernetes" $kubernetes -}}
{{- else if $connection.githubActions -}}
{{- $github := omit $connection.githubActions "tokenSecret" -}}
{{- $_ := set $github "tokenFile" (printf "/run/secrets/gradient-autoscaler/%s/github-token" $name) -}}
{{- $_ := set $entry "githubActions" $github -}}
{{- else -}}
{{- fail (printf "connection %s: set exactly one of kubernetes and githubActions" $name) -}}
{{- end -}}
{{- $connections = append $connections $entry -}}
{{- end -}}
{{- dict
  "listen" (printf "0.0.0.0:%d" (int .Values.service.port))
  "publicUrl" .Values.publicUrl
  "stateDir" "/var/lib/gradient-autoscaler"
  "catchUpInterval" (int .Values.catchUpInterval)
  "connections" $connections
  | toPrettyJson -}}
{{- end -}}
