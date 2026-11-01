{{/*
OAuth proxy image. An explicit oauthProxy.image wins. Otherwise use the cluster's own oauth-proxy: OpenShift ships it
as ImageStream openshift/oauth-proxy, kept at the cluster's version across upgrades and pullable with the cluster's
global pull secret. The lookup runs at install/upgrade time; `helm template` (no cluster) gets the fallback.
*/}}
{{- define "flyops.oauthProxyImage" -}}
{{- if .Values.oauthProxy.image -}}
{{- .Values.oauthProxy.image -}}
{{- else -}}
{{- $img := "" -}}
{{- $is := dict -}}
{{- if .Capabilities.APIVersions.Has "image.openshift.io/v1/ImageStream" -}}
{{- $is = lookup "image.openshift.io/v1" "ImageStream" "openshift" "oauth-proxy" -}}
{{- end -}}
{{- if $is -}}
{{- range (($is.status).tags | default list) -}}
{{- if and (not $img) .items -}}
{{- $img = (index .items 0).dockerImageReference -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $img | default .Values.oauthProxy.fallbackImage -}}
{{- end -}}
{{- end -}}
