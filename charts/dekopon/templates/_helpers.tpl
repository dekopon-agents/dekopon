{{/*
Name helpers.
*/}}
{{- define "dekopon.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "dekopon.fullname" -}}
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

{{- define "dekopon.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "dekopon.labels" -}}
helm.sh/chart: {{ include "dekopon.chart" . }}
{{ include "dekopon.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: dekopon
{{- end -}}

{{- define "dekopon.selectorLabels" -}}
app.kubernetes.io/name: {{ include "dekopon.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Image reference. A digest wins over a tag; an empty tag means "v" + appVersion, because the
container-image workflow publishes under the Git tag and the Git tag carries the v.
*/}}
{{- define "dekopon.image" -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository (default (printf "v%s" .Chart.AppVersion) .Values.image.tag) -}}
{{- end -}}
{{- end -}}

{{- define "dekopon.initImage" -}}
{{- if .Values.initImage.digest -}}
{{- printf "%s@%s" .Values.initImage.repository .Values.initImage.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.initImage.repository .Values.initImage.tag -}}
{{- end -}}
{{- end -}}

{{/*
Object names.
*/}}
{{- define "dekopon.configSecretName" -}}
{{- printf "%s-config" (include "dekopon.fullname" .) -}}
{{- end -}}

{{- define "dekopon.credentialsSecretName" -}}
{{- printf "%s-credentials" (include "dekopon.fullname" .) -}}
{{- end -}}

{{- define "dekopon.catalogConfigMapName" -}}
{{- printf "%s-catalog" (include "dekopon.fullname" .) -}}
{{- end -}}

{{- define "dekopon.stateClaimName" -}}
{{- if .Values.state.existingClaim -}}
{{- .Values.state.existingClaim -}}
{{- else -}}
{{- printf "%s-state" (include "dekopon.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "dekopon.providerStorageClaimName" -}}
{{- if .Values.providerStorage.existingClaim -}}
{{- .Values.providerStorage.existingClaim -}}
{{- else -}}
{{- printf "%s-provider-storage" (include "dekopon.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/*
Whether each optional file has a source at all.
*/}}
{{- define "dekopon.hasPolicies" -}}
{{- if or .Values.broker.policies.inline .Values.broker.policies.existingSecret -}}true{{- end -}}
{{- end -}}

{{- define "dekopon.hasCredentials" -}}
{{- if or .Values.broker.credentials.inline .Values.broker.credentials.existingSecret -}}true{{- end -}}
{{- end -}}

{{- define "dekopon.hasInlineConfig" -}}
{{- if or .Values.broker.config.inline .Values.broker.policies.inline (and .Values.gateway.enabled .Values.gateway.config.inline) -}}true{{- end -}}
{{- end -}}

{{- define "dekopon.chatgptSecretName" -}}
{{- printf "%s-chatgpt" (include "dekopon.fullname" .) -}}
{{- end -}}

{{/*
The ChatGPT credential directory, always a subdirectory of the persistent claim. Composed rather
than configured: an emptyDir here would turn seed-once into seed-per-reschedule.
*/}}
{{- define "dekopon.chatgptDir" -}}
{{- printf "%s/%s" .Values.paths.stateDir .Values.gateway.chatgpt.subdir -}}
{{- end -}}

{{- define "dekopon.chatgptEnabled" -}}
{{- if and .Values.gateway.enabled .Values.gateway.chatgpt.enabled -}}true{{- end -}}
{{- end -}}

{{/*
The seed-once file, as a YAML list of zero or one {file, secret, key}. Kept apart from
managedFiles because the handling is the opposite: managedFiles are overwritten on every start,
this one must survive every start after the first.
*/}}
{{- define "dekopon.seedFiles" -}}
{{- if include "dekopon.chatgptEnabled" . }}
- file: {{ .Values.gateway.chatgpt.fileName }}
  secret: {{ default (include "dekopon.chatgptSecretName" .) .Values.gateway.chatgpt.existingSecret }}
  key: {{ if .Values.gateway.chatgpt.existingSecret }}{{ .Values.gateway.chatgpt.existingSecretKey }}{{ else }}{{ .Values.gateway.chatgpt.fileName }}{{ end }}
{{- end }}
{{- end -}}

{{/*
The files the init container turns into real owner-only regular files, as a YAML list of
{file, secret, key}. The projected source volume and the init script both iterate this, so they
cannot drift apart.
*/}}
{{- define "dekopon.managedFiles" -}}
- file: broker.yaml
  secret: {{ default (include "dekopon.configSecretName" .) .Values.broker.config.existingSecret }}
  key: {{ if .Values.broker.config.existingSecret }}{{ .Values.broker.config.existingSecretKey }}{{ else }}broker.yaml{{ end }}
{{- if include "dekopon.hasPolicies" . }}
- file: policies.cedar
  secret: {{ default (include "dekopon.configSecretName" .) .Values.broker.policies.existingSecret }}
  key: {{ if .Values.broker.policies.existingSecret }}{{ .Values.broker.policies.existingSecretKey }}{{ else }}policies.cedar{{ end }}
{{- end }}
{{- if include "dekopon.hasCredentials" . }}
- file: broker-credentials.yaml
  secret: {{ default (include "dekopon.credentialsSecretName" .) .Values.broker.credentials.existingSecret }}
  key: {{ if .Values.broker.credentials.existingSecret }}{{ .Values.broker.credentials.existingSecretKey }}{{ else }}broker-credentials.yaml{{ end }}
{{- end }}
{{- if .Values.gateway.enabled }}
- file: dekopond.yaml
  secret: {{ default (include "dekopon.configSecretName" .) .Values.gateway.config.existingSecret }}
  key: {{ if .Values.gateway.config.existingSecret }}{{ .Values.gateway.config.existingSecretKey }}{{ else }}dekopond.yaml{{ end }}
{{- end }}
{{- end -}}

{{/*
Startup guards. Every one of these is a configuration mistake whose only other symptom is a pod
that starts and then refuses to serve, which is much harder to read than a template error.
*/}}
{{- define "dekopon.validate" -}}
{{- $uid := .Values.podSecurityContext.runAsUser | int -}}
{{- if and (ne $uid 65532) (eq .Values.image.repository "ghcr.io/dekopon-agents/dekopon") -}}
{{- fail (printf "podSecurityContext.runAsUser is %d, but %s bakes /opt/dekopon/providers/*.wasm owned by 65532 and dekopon-brokerd loads a provider only when its owner equals the broker's own euid. Every provider would fail to load. Use 65532, or supply an image that bakes providers under %d." $uid .Values.image.repository $uid) -}}
{{- end -}}

{{- if and .Values.broker.config.inline .Values.broker.config.existingSecret -}}
{{- fail "broker.config.inline and broker.config.existingSecret are mutually exclusive" -}}
{{- end -}}
{{- if and (not .Values.broker.config.inline) (not .Values.broker.config.existingSecret) -}}
{{- fail "a broker.yaml is required: set broker.config.inline or broker.config.existingSecret" -}}
{{- end -}}

{{- if and .Values.broker.policies.inline .Values.broker.policies.existingSecret -}}
{{- fail "broker.policies.inline and broker.policies.existingSecret are mutually exclusive" -}}
{{- end -}}
{{- if and .Values.broker.credentials.inline .Values.broker.credentials.existingSecret -}}
{{- fail "broker.credentials.inline and broker.credentials.existingSecret are mutually exclusive" -}}
{{- end -}}

{{/* Only inline configuration can be inspected; an existing Secret is opaque here. */}}
{{- if .Values.broker.config.inline -}}
{{- if and (regexMatch "(?m)^[[:space:]]*policiesPath:" .Values.broker.config.inline) (not (include "dekopon.hasPolicies" .)) -}}
{{- fail "broker.config.inline names policiesPath but no policy set was supplied; set broker.policies.inline or broker.policies.existingSecret" -}}
{{- end -}}
{{- if and (regexMatch "(?m)^[[:space:]]*credentialsPath:" .Values.broker.config.inline) (not (include "dekopon.hasCredentials" .)) -}}
{{- fail "broker.config.inline names credentialsPath but no credentials were supplied; set broker.credentials.inline or broker.credentials.existingSecret" -}}
{{- end -}}
{{- if and (regexMatch "(?m)^[[:space:]]*constraintSets:" .Values.broker.config.inline) (not (include "dekopon.hasPolicies" .)) -}}
{{- fail "broker.config.inline declares constraintSets but no policy set was supplied; dekopon-brokerd refuses to start with executable capabilities and no policy" -}}
{{- end -}}
{{- end -}}

{{- if .Values.gateway.enabled -}}
{{- if and .Values.gateway.config.inline .Values.gateway.config.existingSecret -}}
{{- fail "gateway.config.inline and gateway.config.existingSecret are mutually exclusive" -}}
{{- end -}}
{{- if and (not .Values.gateway.config.inline) (not .Values.gateway.config.existingSecret) -}}
{{- fail "gateway.enabled is true, so a dekopond.yaml is required: set gateway.config.inline or gateway.config.existingSecret" -}}
{{- end -}}
{{- if and .Values.gateway.catalog.inline .Values.gateway.catalog.existingConfigMap -}}
{{- fail "gateway.catalog.inline and gateway.catalog.existingConfigMap are mutually exclusive" -}}
{{- end -}}
{{- if and (not .Values.gateway.catalog.inline) (not .Values.gateway.catalog.existingConfigMap) -}}
{{- fail "gateway.enabled is true, so an agent catalog is required: set gateway.catalog.inline or gateway.catalog.existingConfigMap" -}}
{{- end -}}
{{- end -}}

{{- if include "dekopon.chatgptEnabled" . -}}
{{- if and .Values.gateway.chatgpt.inline .Values.gateway.chatgpt.existingSecret -}}
{{- fail "gateway.chatgpt.inline and gateway.chatgpt.existingSecret are mutually exclusive" -}}
{{- end -}}
{{- if and (not .Values.gateway.chatgpt.inline) (not .Values.gateway.chatgpt.existingSecret) -}}
{{- fail "gateway.chatgpt.enabled is true, so a credential is required: set gateway.chatgpt.inline or gateway.chatgpt.existingSecret. Produce one with `dekopon auth chatgpt export`." -}}
{{- end -}}
{{- if not (regexMatch "^[A-Za-z0-9._-]+$" .Values.gateway.chatgpt.subdir) -}}
{{- fail (printf "gateway.chatgpt.subdir must be one path segment joined onto paths.stateDir, got %q; the credential has to live on the claim" .Values.gateway.chatgpt.subdir) -}}
{{- end -}}
{{- if or (eq .Values.gateway.chatgpt.subdir ".") (eq .Values.gateway.chatgpt.subdir "..") -}}
{{- fail "gateway.chatgpt.subdir must not be . or .." -}}
{{- end -}}
{{- if not (regexMatch "^[A-Za-z0-9._-]+$" .Values.gateway.chatgpt.fileName) -}}
{{- fail (printf "gateway.chatgpt.fileName must be one path segment, got %q" .Values.gateway.chatgpt.fileName) -}}
{{- end -}}
{{- end -}}
{{- if and .Values.gateway.chatgpt.enabled (not .Values.gateway.enabled) -}}
{{- fail "gateway.chatgpt.enabled has no effect without gateway.enabled: the credential is read by dekopond, not by the broker" -}}
{{- end -}}

{{- $chartPaths := dict "paths.configDir" .Values.paths.configDir "paths.runtimeDir" .Values.paths.runtimeDir "paths.stateDir" .Values.paths.stateDir "paths.catalogDir" .Values.paths.catalogDir -}}
{{- range $name, $path := $chartPaths -}}
{{- if or (not (regexMatch "^/([A-Za-z0-9._-]+/)*[A-Za-z0-9._-]+$" $path)) (ne (clean $path) $path) -}}
{{- fail (printf "%s must be a canonical absolute path of safe non-dot segments with no repeated or trailing slash, got %q" $name $path) -}}
{{- end -}}
{{- range $segment := splitList "/" $path -}}
{{- if or (eq $segment ".") (eq $segment "..") -}}
{{- fail (printf "%s must not contain . or .. segments, got %q" $name $path) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- range $leftName, $leftPath := $chartPaths -}}
{{- range $rightName, $rightPath := $chartPaths -}}
{{- if and (ne $leftName $rightName) (or (eq (clean $leftPath) (clean $rightPath)) (hasPrefix (printf "%s/" (clean $leftPath)) (clean $rightPath)) (hasPrefix (printf "%s/" (clean $rightPath)) (clean $leftPath))) -}}
{{- fail (printf "%s (%s) and %s (%s) must not overlap; separate volume mounts would shadow files" $leftName $leftPath $rightName $rightPath) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- if .Values.providerStorage.enabled -}}
{{- if not .Values.providerStorage.existingKeySecret -}}
{{- fail "providerStorage.enabled requires operator-managed providerStorage.existingKeySecret; the chart never owns or deletes the namespace key" -}}
{{- end -}}
{{- if eq (include "dekopon.providerStorageClaimName" .) (include "dekopon.stateClaimName" .) -}}
{{- fail "provider storage and audit state must use distinct PersistentVolumeClaims" -}}
{{- end -}}
{{- range $name, $value := dict "providerStorage.keyFileName" .Values.providerStorage.keyFileName "providerStorage.existingKeySecretKey" .Values.providerStorage.existingKeySecretKey -}}
{{- if or (not (regexMatch "^[A-Za-z0-9._-]+$" $value)) (eq $value ".") (eq $value "..") -}}
{{- fail (printf "%s must be one non-dot path segment" $name) -}}
{{- end -}}
{{- end -}}
{{- range $name, $path := dict "providerStorage.rootPath" .Values.providerStorage.rootPath "providerStorage.keyDir" .Values.providerStorage.keyDir -}}
{{- if or (not (regexMatch "^/([A-Za-z0-9._-]+/)*[A-Za-z0-9._-]+$" $path)) (ne (clean $path) $path) -}}
{{- fail (printf "%s must be a canonical absolute path of safe non-empty segments with no repeated or trailing slash, got %q" $name $path) -}}
{{- end -}}
{{- range $segment := splitList "/" $path -}}
{{- if or (eq $segment ".") (eq $segment "..") -}}
{{- fail (printf "%s must not contain . or .. segments, got %q" $name $path) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $storageMounts := dict "providerStorage.rootPath" (clean .Values.providerStorage.rootPath) "providerStorage.keyDir" (clean .Values.providerStorage.keyDir) -}}
{{- $ownedMounts := dict "paths.configDir" (clean .Values.paths.configDir) "paths.runtimeDir" (clean .Values.paths.runtimeDir) "paths.stateDir" (clean .Values.paths.stateDir) "paths.catalogDir" (clean .Values.paths.catalogDir) "temporary directory" "/tmp" "projected configuration source" "/dekopon-source" "projected storage-key source" "/dekopon-storage-key-source" "packaged default providers" "/opt/dekopon/providers" "packaged optional providers" "/opt/dekopon/optional-providers" "packaged executables" "/usr/local/bin" "packaged documentation" "/usr/share/doc/dekopon" -}}
{{- range $storageName, $storagePath := $storageMounts -}}
{{- range $ownedName, $ownedPath := $ownedMounts -}}
{{- if or (eq $storagePath $ownedPath) (hasPrefix (printf "%s/" $storagePath) $ownedPath) (hasPrefix (printf "%s/" $ownedPath) $storagePath) -}}
{{- fail (printf "%s (%s) must not equal, contain, or be contained by chart-owned %s (%s); overlapping volume mounts shadow files" $storageName $storagePath $ownedName $ownedPath) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if or (eq (clean .Values.providerStorage.rootPath) (clean .Values.providerStorage.keyDir)) (hasPrefix (printf "%s/" (clean .Values.providerStorage.rootPath)) (clean .Values.providerStorage.keyDir)) (hasPrefix (printf "%s/" (clean .Values.providerStorage.keyDir)) (clean .Values.providerStorage.rootPath)) -}}
{{- fail "providerStorage.rootPath and providerStorage.keyDir must not overlap" -}}
{{- end -}}
{{- end -}}

{{- range $pathName, $path := $chartPaths -}}
{{- range $ownedName, $ownedPath := dict "temporary directory" "/tmp" "projected configuration source" "/dekopon-source" "projected storage-key source" "/dekopon-storage-key-source" "packaged default providers" "/opt/dekopon/providers" "packaged optional providers" "/opt/dekopon/optional-providers" "packaged executables" "/usr/local/bin" "packaged documentation" "/usr/share/doc/dekopon" -}}
{{- if or (eq (clean $path) $ownedPath) (hasPrefix (printf "%s/" (clean $path)) $ownedPath) (hasPrefix (printf "%s/" $ownedPath) (clean $path)) -}}
{{- fail (printf "%s (%s) must not overlap chart/image-owned %s (%s)" $pathName $path $ownedName $ownedPath) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The broker container. Emitted either as a native sidecar (an initContainer with
restartPolicy: Always, so it starts and passes its startup probe before dekopond runs and is torn
down after dekopond stops) or, when the gateway is disabled, as the pod's only regular container.
Arguments: dict "ctx" $ "sidecar" bool
*/}}
{{- define "dekopon.brokerContainer" -}}
{{- $ := .ctx -}}
- name: broker
  {{- if .sidecar }}
  # Native sidecar. This is the ordering primitive: Kubernetes does not start dekopond until this
  # container's startup probe succeeds, and it terminates this container only after dekopond has
  # stopped. Without it the gateway races the broker, and dekopond exits non-zero when the socket
  # is absent, so the pod would crash-loop its way to a working state.
  restartPolicy: Always
  {{- end }}
  image: {{ include "dekopon.image" $ }}
  imagePullPolicy: {{ $.Values.image.pullPolicy }}
  # No ENTRYPOINT in the image: the command selects which of the four binaries runs.
  command: ["dekopon-brokerd"]
  args: ["--config", "{{ $.Values.paths.configDir }}/broker.yaml"]
  securityContext:
    {{- toYaml $.Values.securityContext | nindent 4 }}
  env:
    {{- with $.Values.broker.env }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with $.Values.broker.envFrom }}
  envFrom:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  # Both probes are a real broker client over the real socket. `capabilities` is evaluated from
  # policy and the constraint catalog and appends no audit record, so probing does not consume the
  # audit log's bounded record budget.
  startupProbe:
    exec:
      command: ["dekopon-run", "broker", "capabilities", "--socket", "{{ $.Values.paths.runtimeDir }}/broker.sock"]
    {{- toYaml $.Values.broker.startupProbe | nindent 4 }}
  readinessProbe:
    exec:
      command: ["dekopon-run", "broker", "capabilities", "--socket", "{{ $.Values.paths.runtimeDir }}/broker.sock"]
    {{- toYaml $.Values.broker.readinessProbe | nindent 4 }}
  resources:
    {{- toYaml $.Values.broker.resources | nindent 4 }}
  volumeMounts:
    - name: config
      mountPath: {{ $.Values.paths.configDir }}
      readOnly: true
    - name: runtime
      mountPath: {{ $.Values.paths.runtimeDir }}
    - name: state
      mountPath: {{ $.Values.paths.stateDir }}
{{- if $.Values.providerStorage.enabled }}
    - name: provider-storage
      mountPath: {{ $.Values.providerStorage.rootPath }}
    - name: provider-storage-key
      mountPath: {{ $.Values.providerStorage.keyDir }}
      readOnly: true
{{- end }}
    - name: tmp
      mountPath: /tmp
{{- end -}}
