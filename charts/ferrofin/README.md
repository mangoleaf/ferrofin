# Ferrofin Helm chart

Official chart for the [Ferrofin](https://example.com/ferrofin) media server — a Rust implementation of the Jellyfin server API.

The value contract mirrors the common subset of the upstream
[jellyfin-helm](https://github.com/jellyfin/jellyfin-helm) chart, so moving a Jellyfin
release to Ferrofin is near-zero churn. Two intentional differences:

- the app config block is `ferrofin:` (not `jellyfin:`), with no `enableDLNA` (Ferrofin has no DLNA);
- config mounts at `/data` (Ferrofin's data dir), not `/config`.

## Install

The chart is published as an OCI artifact next to the image:

```bash
helm install ferrofin oci://<registry>/ferrofin/charts/ferrofin \
  --version 0.1.0 -n ferrofin --create-namespace -f my-values.yaml
```

For a private registry, provide `imagePullSecrets` (a `docker-registry` secret) in your values.

## Key values

| Key | Default | Purpose |
|---|---|---|
| `image.repository` / `image.tag` | see `values.yaml` / chart appVersion | Server image |
| `imagePullSecrets` | `[]` | Secrets for the private registry |
| `service.port` | `8096` | Port Ferrofin listens on (also the container port) |
| `persistence.config.enabled` | `true` | Persist the data dir; `false` → emptyDir |
| `persistence.config.mountPath` | `/data` | Where the data dir mounts |
| `persistence.config.existingClaim` | `""` | Use an existing PVC instead of a chart-created one |
| `volumes` / `volumeMounts` | `[]` | Extra media/host volumes |
| `ferrofin.env` / `ferrofin.envFrom` / `ferrofin.args` | `[]` | Config via `FERROFIN_*` env or CLI flags |
| `livenessProbe` / `readinessProbe` | `/health/live` / `/health/ready` | Ferrofin's health endpoints |
| `ingress.enabled` | `false` | Standard `Ingress` (most clusters expose Ferrofin this way) |
| `httpRoute.enabled` | `false` | Gateway API `HTTPRoute` (alternative to ingress) |
| `networkPolicy.enabled` | `false` | Pod isolation (needs a policy-enforcing CNI) |
| `serviceAccount.create` | `true` | Dedicated service account |
| `metrics.serviceMonitor.enabled` | `false` | Off — Ferrofin exposes no `/metrics` yet |

To expose Ferrofin on most clusters, enable ingress:

```yaml
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: ferrofin.example.com
      paths: [{ path: /, pathType: Prefix }]
  tls:
    - secretName: ferrofin-tls
      hosts: [ferrofin.example.com]
```

Health probes target Ferrofin's real endpoints (`GET /health/live`, `GET /health/ready`);
there is no Jellyfin-style `/health`.
