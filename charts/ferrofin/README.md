# Ferrofin Helm chart

Official chart for the [Ferrofin](https://github.com/mangoleaf/ferrofin) media server — a Rust implementation of the Jellyfin server API.

The value contract mirrors the common subset of the upstream
[jellyfin-helm](https://github.com/jellyfin/jellyfin-helm) chart, so moving a Jellyfin
release to Ferrofin is near-zero churn. Two intentional differences:

- the app config block is `ferrofin:` (not `jellyfin:`), with no `enableDLNA` (Ferrofin has no DLNA);
- config mounts at `/data` (Ferrofin's data dir), not `/config`.

## Install

The chart is published as an OCI artifact next to the image:

```bash
helm install ferrofin oci://ghcr.io/mangoleaf/ferrofin/charts/ferrofin \
  --version <chart-version> -n ferrofin --create-namespace -f my-values.yaml
```

The chart version equals the Ferrofin release it ships (`v1.2.3` → chart `1.2.3`). The
image is public on GHCR, so no pull secret is needed; if you mirror it into a private
registry, set `image.repository` and `imagePullSecrets` in your values.

## Key values

| Key | Default | Purpose |
|---|---|---|
| `image.repository` / `image.tag` | see `values.yaml` / chart appVersion | Server image |
| `imagePullSecrets` | `[]` | Secrets for the private registry |
| `discovery.enabled` | `false` | Declare UDP 7359 and create a separate discovery Service; allow it through the chart NetworkPolicy |
| `discovery.service.type` / `.annotations` | `ClusterIP` / `{}` | UDP Service type and provider annotations |
| `hostNetwork` | `false` | Share node networking for LAN broadcasts; requires `strategy.type: Recreate` |
| `service.port` | `8096` | Port Ferrofin listens on (also the container port) |
| `persistence.config.enabled` | `true` | Persist the data dir; `false` → emptyDir |
| `persistence.config.mountPath` | `/data` | Where the data dir mounts |
| `persistence.config.existingClaim` | `""` | Use an existing PVC instead of a chart-created one |
| `volumes` / `volumeMounts` | `[]` | Extra media/host volumes |
| `ferrofin.env` / `ferrofin.envFrom` / `ferrofin.args` | `[]` | Config via `FERROFIN_*` env or CLI flags |
| `livenessProbe` / `readinessProbe` | `/health/live` / `/health/ready` | Ferrofin's health endpoints |
| `strategy` | RollingUpdate, surge 1 / unavailable 0 | Upgrade without dropping playback; set `type: Recreate` if the config PVC can't be mounted twice |
| `ingress.enabled` | `false` | Standard `Ingress` (most clusters expose Ferrofin this way) |
| `httpRoute.enabled` | `false` | Gateway API `HTTPRoute` (alternative to ingress) |
| `networkPolicy.enabled` | `false` | Pod isolation (needs a policy-enforcing CNI) |
| `serviceAccount.create` | `true` | Dedicated service account |
| `metrics.enabled` / `metrics.serviceMonitor.enabled` | `false` | Scrape Ferrofin's Prometheus `/metrics` endpoint via a `ServiceMonitor` (enable metrics in the app too: `FERROFIN_ENABLE_METRICS=true`) |
| `dashboards.enabled` | `false` | Ship the Grafana dashboards as a ConfigMap for a Grafana dashboard sidecar |
| `dashboards.label` / `dashboards.labelValue` | `grafana_dashboard` / `"1"` | The label the sidecar selects dashboard ConfigMaps by |
| `dashboards.folderAnnotation` / `dashboards.folder` | `grafana_folder` / `Ferrofin` | The annotation the sidecar reads the Grafana folder from, and the folder (`""` = no annotation) |
| `dashboards.namespace` | `""` (release namespace) | Where the ConfigMap goes, for a sidecar that watches one namespace |

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

## Grafana dashboards

With `dashboards.enabled`, the chart renders one ConfigMap holding Ferrofin's three
Grafana dashboards — **Golden Signals** (`ferrofin-golden-signals`), **Deep Dive**
(`ferrofin-deep-dive`) and **Library Scans** (`ferrofin-library-scans`) — for the Grafana
dashboard sidecar that the `grafana` and `kube-prometheus-stack` charts ship
(`sidecar.dashboards.enabled: true`). The sidecar loads every ConfigMap carrying its
label and files it in the folder named by its folder annotation; the defaults match
the sidecar's usual `grafana_dashboard: "1"` label and `grafana_folder` annotation:

```yaml
metrics:
  enabled: true
  serviceMonitor:
    enabled: true
dashboards:
  enabled: true
  folder: Ferrofin
ferrofin:
  config:
    FERROFIN_ENABLE_METRICS: "true"
```

The dashboards pick their Prometheus through a `datasource` variable, so they need no
datasource uid, and every query is scoped to a `job` variable (Golden Signals and Deep
Dive default to the job `ferrofin`, which the ServiceMonitor produces for a release
named `ferrofin`). What each panel shows, and what healthy looks like, is in
[`contrib/metrics/README.md`](../../contrib/metrics/README.md). Server-side metric
settings are ordinary environment variables, e.g. the library-scan histogram buckets:

```yaml
ferrofin:
  config:
    FERROFIN_METRICS_SCAN_DURATION_BUCKETS: "0.01,0.1,1,10,60,300,1800,7200"
```

The JSON files under `dashboards/` are **copies** of `contrib/metrics/grafana-*.json`:
Helm reads only files inside the chart, and tools that render the chart straight from
git (Argo CD, for one) reject a symlink that points outside it. After editing a dashboard in `contrib/metrics/`, copy it
over (`cp contrib/metrics/grafana-*.json charts/ferrofin/dashboards/`); the
`dashboards` test in `apps/ferrofin-server` fails while the copies differ.

## LAN server discovery

Ferrofin answers Jellyfin discovery requests on UDP 7359. `discovery.enabled`
controls chart exposure only; the persisted network setting `AutoDiscovery` controls
whether the application listens and takes effect after restart. The chart creates a
separate UDP Service so HTTP LoadBalancers need not support mixed protocols.
ClusterIP, NodePort, LoadBalancer, ingress and HTTPRoute do not themselves carry LAN
broadcasts to a pod.

For a node attached to the clients' LAN, opt into host networking:

```yaml
hostNetwork: true
strategy:
  type: Recreate
discovery:
  enabled: true
ferrofin:
  config:
    FERROFIN_PUBLISHED_URL: "http://192.168.1.10:8096"
```

Replace the example address with the selected node's reachable LAN address, and use
`nodeSelector` or affinity to keep the pod on that node. The chart sets
`dnsPolicy: ClusterFirstWithHostNet`. Keep one replica and leave TCP 8096 and UDP
7359 free on the node; `Recreate` prevents an upgrade's two pods from competing for
those ports. Node firewall rules must permit the traffic. With host networking,
NetworkPolicy enforcement depends on the CNI; apply host firewall policy as needed.
For ordinary pod networking, the discovery NetworkPolicy rule follows the same
source selectors as HTTP; restrictive selectors may exclude external LAN clients.

A LAN-attached pod network is another option if the CNI delivers broadcasts and
clients can route to the advertised HTTP address. See [deployment and verification
instructions](../../docs/SERVER_DISCOVERY.md). Test from a separate LAN client;
unicast access to a Service IP alone does not establish broadcast discovery.
