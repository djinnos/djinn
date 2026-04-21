{{/*
Return the chart's fullname — used as the stem for all resource names so
multiple releases in the same namespace don't collide.
*/}}
{{- define "djinn.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Return the chart name (sanitised).
*/}}
{{- define "djinn.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Resolve the namespace all resources land in. Defaults to the Release namespace
when values.namespace.name is empty.
*/}}
{{- define "djinn.namespace" -}}
{{- default .Release.Namespace .Values.namespace.name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels applied to every chart-managed resource.
*/}}
{{- define "djinn.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "djinn.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: djinn
{{- with .Values.labels }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/*
Selector labels — the subset of labels that should drive Service/Deployment
selectors. Must stay stable across upgrades.
*/}}
{{- define "djinn.selectorLabels" -}}
app.kubernetes.io/name: {{ include "djinn.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Server component labels.
*/}}
{{- define "djinn.server.labels" -}}
{{ include "djinn.labels" . }}
app.kubernetes.io/component: server
{{- end -}}

{{- define "djinn.server.selectorLabels" -}}
{{ include "djinn.selectorLabels" . }}
app.kubernetes.io/component: server
{{- end -}}

{{/*
Dolt component labels.
*/}}
{{- define "djinn.dolt.labels" -}}
{{ include "djinn.labels" . }}
app.kubernetes.io/component: dolt
{{- end -}}

{{- define "djinn.dolt.selectorLabels" -}}
{{ include "djinn.selectorLabels" . }}
app.kubernetes.io/component: dolt
{{- end -}}

{{/*
Qdrant component labels.
*/}}
{{- define "djinn.qdrant.labels" -}}
{{ include "djinn.labels" . }}
app.kubernetes.io/component: qdrant
{{- end -}}

{{- define "djinn.qdrant.selectorLabels" -}}
{{ include "djinn.selectorLabels" . }}
app.kubernetes.io/component: qdrant
{{- end -}}

{{/*
ServiceAccount names — fully-qualified with the release prefix so multiple
releases don't collide in a shared namespace.
*/}}
{{- define "djinn.serviceAccountName.controller" -}}
{{- printf "%s-%s" (include "djinn.fullname" .) .Values.serviceAccount.controller | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "djinn.serviceAccountName.taskrun" -}}
{{- printf "%s-%s" (include "djinn.fullname" .) .Values.serviceAccount.taskrun | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Names for chart-managed Secrets. When values.secrets.*.existingSecret is set,
that string wins; otherwise we use the chart-local name.
*/}}
{{- define "djinn.secretName.githubApp" -}}
{{- if .Values.secrets.githubApp.existingSecret -}}
{{- .Values.secrets.githubApp.existingSecret -}}
{{- else -}}
{{- printf "%s-github-app" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "djinn.secretName.vaultKey" -}}
{{- if .Values.secrets.vaultKey.existingSecret -}}
{{- .Values.secrets.vaultKey.existingSecret -}}
{{- else -}}
{{- printf "%s-vault-key" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "djinn.secretName.providers" -}}
{{- if .Values.secrets.providers.existingSecret -}}
{{- .Values.secrets.providers.existingSecret -}}
{{- else -}}
{{- printf "%s-providers" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/*
Image-pipeline DNS helpers — all derive from release fullname + namespace so
we never assume a release name like "djinn". `imagePipeline.registryHost` in
values.yaml is treated as an override; when empty (the expected case) we
compute the in-cluster Zot Service DNS.

These helpers are the single source of truth consumed by:
  * registry-auth-secret.yaml — sets the `auths.<host>` key in config.json
  * buildkitd-configmap.yaml  — sets the `[registry."<host>"]` block
  * deployment-server.yaml    — injects DJINN_IMAGE_{REGISTRY,BUILDKITD}_HOST
                                env vars so the server reads the chart-
                                computed value instead of the Rust defaults.

External registries (ECR, GHCR) override `imagePipeline.registryHost`
directly and the in-cluster Zot Service name is ignored.
*/}}
{{- define "djinn.imagePipeline.registryHost" -}}
{{- if .Values.imagePipeline.registryHost -}}
{{- .Values.imagePipeline.registryHost -}}
{{- else -}}
{{- printf "%s-zot.%s.svc.cluster.local:%d"
      (include "djinn.fullname" .)
      (include "djinn.namespace" .)
      (.Values.imagePipeline.zot.port | int) -}}
{{- end -}}
{{- end -}}

{{- define "djinn.imagePipeline.buildkitdHost" -}}
{{- if .Values.imagePipeline.buildkitdHost -}}
{{- .Values.imagePipeline.buildkitdHost -}}
{{- else -}}
{{- printf "tcp://%s-buildkitd.%s.svc.cluster.local:%d"
      (include "djinn.fullname" .)
      (include "djinn.namespace" .)
      (.Values.imagePipeline.buildkitd.port | int) -}}
{{- end -}}
{{- end -}}

{{- define "djinn.imagePipeline.registryAuthSecretName" -}}
{{- if .Values.imagePipeline.zot.auth.existingSecret -}}
{{- .Values.imagePipeline.zot.auth.existingSecret -}}
{{- else -}}
{{- printf "%s-zot-auth" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- /* Back-compat alias — canonical source is djinn.pvcName.mirrors. */ -}}
{{- define "djinn.imagePipeline.mirrorPvcName" -}}
{{- include "djinn.pvcName.mirrors" . -}}
{{- end -}}

{{- define "djinn.secretName.langfuse" -}}
{{- if .Values.langfuse.existingSecret -}}
{{- .Values.langfuse.existingSecret -}}
{{- else -}}
{{- printf "%s-langfuse" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/*
Service names (used inside DJINN_MYSQL_URL / QDRANT_URL env in configmap).
*/}}
{{- define "djinn.serviceName.server" -}}
{{- printf "%s-server" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "djinn.serviceName.dolt" -}}
{{- printf "%s-dolt" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "djinn.serviceName.qdrant" -}}
{{- printf "%s-qdrant" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
PVC names. Respect values.storage.*.existingClaim — when set, skip rendering
the PVC template and point the Deployment volume at the caller-provided name.
*/}}
{{- define "djinn.pvcName.mirrors" -}}
{{- if .Values.storage.mirrors.existingClaim -}}
{{- .Values.storage.mirrors.existingClaim -}}
{{- else -}}
{{- printf "%s-mirrors" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "djinn.pvcName.cache" -}}
{{- if .Values.storage.cache.existingClaim -}}
{{- .Values.storage.cache.existingClaim -}}
{{- else -}}
{{- printf "%s-cache" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "djinn.pvcName.projects" -}}
{{- if .Values.storage.projects.existingClaim -}}
{{- .Values.storage.projects.existingClaim -}}
{{- else -}}
{{- printf "%s-projects" (include "djinn.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
