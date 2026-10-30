# Ferrofin Helm chart

Official chart for the [Ferrofin](https://github.com/mangoleaf/ferrofin) media server — a Rust implementation of the Jellyfin server API.

The value contract mirrors the common subset of the upstream
[jellyfin-helm](https://github.com/jellyfin/jellyfin-helm) chart, so moving a Jellyfin
release to Ferrofin is near-zero churn. Two intentional differences:

- the app config block is `ferrofin:` (not `jellyfin:`), with no `enableDLNA` (Ferrofin has no DLNA);
- config mounts at `/data` (Ferrofin's data dir), not `/config`.

Stop Jellyfin before Ferrofin first boots on its database: adopting a database Jellyfin is
still using is not safe, and adoption is one-way (see
[the upgrade notes](../../docs/UPGRADING.md)).

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
| `persistence.config.accessMode` | `ReadWriteOnce` | Access mode of the chart-created PVC; RWX does not make a rolling update safe across nodes (see [Upgrades and the database](#upgrades-and-the-database)) |
| `volumes` / `volumeMounts` | `[]` | Extra media/host volumes |
| `ferrofin.env` / `ferrofin.envFrom` / `ferrofin.args` | `[]` | Config via `FERROFIN_*` env or CLI flags |
| `livenessProbe` / `readinessProbe` | `/health/live` / `/health/ready` | Ferrofin's health endpoints |
| `strategy` | RollingUpdate, surge 1 / unavailable 0 | Upgrade without dropping playback, with two versions on one database until the new pod is ready; `type: Recreate` stops the old pod first (see [Upgrades and the database](#upgrades-and-the-database)) |
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

A fresh database's `admin` account has no password until the `/web` setup wizard sets one,
so finish the wizard before you enable ingress. For a release named `ferrofin`, reach it with
`kubectl -n ferrofin port-forward svc/ferrofin 8096` and open `http://localhost:8096/web`.
Finish it before you enable an `httpRoute` too. Alternatively, seed the password from a Secret
with a `FERROFIN_ADMIN_PASSWORD` key, through `ferrofin.envFrom`.

Health probes target Ferrofin's real endpoints: `GET /health/live` is process-only, and
`GET /health/ready` checks that the database answers. Jellyfin's own `/health` (plain-text
`Healthy`, or `Unhealthy` with a 503) runs the same check, so healthchecks written for
Jellyfin keep working.

## Upgrades and the database

The default `RollingUpdate` (surge 1, unavailable 0) keeps playback going through an
upgrade because the old pod runs until the new one is ready. Ferrofin's database is one
SQLite file that both pods open, so the two versions overlap, and that has a cost.

**The old version keeps serving and writing for the whole of the new pod's boot.** The new
pod binds its port, then adopts and migrates the database and runs its one-shot repairs, and
only then does `/health/ready` pass. On a small database with nothing to migrate that takes
seconds; a heavy migration on a large database can take minutes. Nothing makes one version
wait for the other: SQLite's locks keep the file intact on one node, but they protect the
file, not the logic. During that window:

- **Rows the old version writes can miss a one-shot repair or data migration for good.**
  Each runs once. A row saved after it ran stays in the old shape; many of these heal on the
  next scan or save.
- **The old version can answer with errors.** A write waits 30 seconds for the lock and then
  fails, so a migration or repair transaction longer than that surfaces as a `500`. A
  transaction that read before it writes can fail straight away when the other version commits
  in between. The old version also runs against a schema it was not built for once a migration
  has changed a table it reads.
- **The old version cannot restart once a migration has been applied.** A database migrated by
  a newer version does not open under an older one, so an old pod that restarts after that point
  stays down until the new pod is ready. For the same reason, `helm rollback` after a migration
  also needs the database restored from a backup.
- **The new pod can fail and restart.** If a repair loses a race with the old version, the boot
  fails and Kubernetes starts the pod again, while the old pod keeps serving. Each migration is
  a transaction, and one that fails leaves nothing behind.

To opt out, stop the old pod first with `type: Recreate`. Only one version ever touches the
database, and playback stops for the whole boot:

```yaml
strategy:
  type: Recreate
```

You can keep the default and stop first only for an upgrade that needs it, by adding
`--set strategy.type=Recreate` to that one `helm upgrade` (with `--reuse-values` it stays set
for later upgrades too). [`docs/UPGRADING.md`](../../docs/UPGRADING.md#kubernetes-stop-the-old-pod-first-for-heavy-migrations)
says when to do that.

### Storage: one node at a time

**Never let two nodes run Ferrofin on one database.** SQLite's WAL mode keeps a shared-memory
index (the `-shm` file) that every process using the database must share, so
[all of them must be on the same host](https://www.sqlite.org/wal.html). Over NFS, CephFS or
any other RWX volume, a surge pod on a second node sees a different index from the old pod,
and that can corrupt the database. The chart cannot tell where your volume lives and does not
check this. What keeps you safe:

| Your volume | Rolling update | What happens |
|---|---|---|
| `ReadWriteOnce` | Works if the surge pod lands on the old pod's node, which the scheduler does not aim for: use the `affinity` below | On any other node the surge pod waits for the volume and the rollout stalls, which does no harm but needs you to fix it |
| `ReadWriteOnce` on node-local storage, or a pod pinned to one node | Works | Both pods are on that node |
| `ReadWriteOncePod` | Stalls | The volume admits one pod, so use `Recreate` |
| `ReadWriteMany` (NFS, CephFS) | **Only with both pods held on one node** | Otherwise the surge pod can land on a second node: use `Recreate` |

To keep a surge pod on the old pod's node, use the `affinity` value. The first pod of a release
still schedules: Kubernetes lets a pod satisfy its own affinity when nothing matches yet. Use
your release's name:

```yaml
affinity:
  podAffinity:
    requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchLabels:
            app.kubernetes.io/instance: <release>
        topologyKey: kubernetes.io/hostname
```

The surge pod then waits if that node has no room for it, so leave room or use `Recreate`.

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
