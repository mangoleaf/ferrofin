# LAN server discovery

Ferrofin answers Jellyfin clients' IPv4 discovery requests on UDP **7359**, returning
its server ID, name and advertised HTTP(S) URL. Discovery is enabled by default.
The persisted network configuration's `AutoDiscovery` setting applies at server
startup; restart the server after changing it. The configuration is stored at
`{config}/users/named/network.json` and exposed through
`GET/POST /System/Configuration/network`. This is Jellyfin server discovery,
separate from DLNA/SSDP and tuner discovery.

Clients must be able to send LAN broadcasts to the server and access the URL in its
reply. Permit inbound UDP 7359 and the configured HTTP port in the server's
firewall. A reverse proxy can provide the advertised HTTP URL, but ordinary HTTP
proxying does not forward UDP discovery. Across routed networks or where broadcasts
are blocked, connect the Jellyfin client manually using the server URL.

When using `FERROFIN_BASE_URL` (for example `/jellyfin`), Ferrofin advertises and
serves the API/web client under that prefix. The bootstrap `base_url` setting in
`config.toml` is equivalent; restart the process to reload either setting. The
named network document's `BaseUrl` does not control the mount. Configure reverse
proxies to preserve the prefix. Requests missing it redirect to the web client. Ferrofin's deployment
probes `/health/live` and `/health/ready` remain available at the root as well as
under the prefix.

## Docker

On a Linux Docker host attached to the clients' LAN, host networking delivers the
broadcasts directly to Ferrofin:

```sh
docker run -d --name ferrofin \
  --network host \
  -e FERROFIN_PUBLISHED_URL=http://192.168.1.10:8096 \
  -e FERROFIN_ADMIN_PASSWORD='<choose-a-password>' \
  -v ferrofin-data:/data \
  -v /path/to/media:/media:ro \
  ghcr.io/mangoleaf/ferrofin:latest
```

Host networking exposes port 8096 on every host interface as soon as the container starts.
A fresh database's `admin` account has no password until the `/web` setup wizard sets one,
so this example seeds it with `FERROFIN_ADMIN_PASSWORD`. The variable is only read when the
first user is created, so you can remove it once that account exists. To use the wizard
instead, run the [loopback-only quickstart](../README.md#quickstart) against the same
`ferrofin-data` volume first and finish setup there, then remove that container and start
this one.

Replace `192.168.1.10` with the host's LAN address. Leave TCP 8096 and UDP 7359 free
on that host; another Jellyfin/Ferrofin instance cannot use the same discovery port.
Do not add `-p` mappings with host networking. This example requires Linux host
networking; it does not establish broadcast support through a desktop VM.

`:latest` follows final releases only. Pin `vX.Y.Z` for a long-running install, because
adoption and migrations are one-way.

A container with its own LAN-attached interface (for example, an appropriately
configured macvlan network) can also receive broadcasts. Ensure the advertised URL
is reachable from the client; set `FERROFIN_PUBLISHED_URL` when automatic interface
selection would expose an internal container address or when using a reverse proxy.

The image's `EXPOSE 7359/udp` is metadata. Docker `-p 7359:7359/udp` publishes a
unicast port and must not be assumed to forward LAN broadcasts into a bridge
network. The default TCP-only quickstart supports manual URL connections.

## Kubernetes

See the [chart's LAN discovery example](../charts/ferrofin/README.md#lan-server-discovery)
for opt-in host networking on a LAN-connected node. The chart's `discovery.enabled`
exposes UDP separately from the HTTP Service; it does not override the persisted
`AutoDiscovery` setting. Kubernetes Service exposure alone does not deliver LAN
broadcasts. An appropriately configured LAN-attached pod interface is an alternative
to host networking.

## Verify from the client network

Run this from a **separate machine on the same LAN**, with Python 3. It broadcasts
the Jellyfin discovery request, prints each response and requests public information
from its advertised URL:

```python
import json
import socket
import urllib.request

with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.settimeout(3)
    sock.bind(("0.0.0.0", 0))
    sock.sendto(b"who is JellyfinServer?", ("255.255.255.255", 7359))
    replies = []
    while True:
        try:
            packet, peer = sock.recvfrom(65535)
        except socket.timeout:
            break
        info = json.loads(packet)
        print(peer, info)
        replies.append(info)

for info in replies:
    url = info["Address"].rstrip("/") + "/System/Info/Public"
    with urllib.request.urlopen(url, timeout=5) as response:
        public = json.load(response)
    assert public["Id"] == info["Id"], (info, public)
    assert public["ServerName"] == info["Name"], (info, public)
    print("Reachable:", url)

assert replies, "No discovery responses received"
```

On a machine with multiple interfaces, bind the socket to its LAN IP or use that
LAN's directed broadcast address. Then verify
that a native Jellyfin client discovers Ferrofin and connects. Repeat with an
explicit published URL and with `AutoDiscovery` disabled followed by restart (the
latter should return no Ferrofin response). Compare Jellyfin sequentially or on a
separate host to avoid port collisions. A successful unicast probe on loopback or a
Service IP verifies the responder, not LAN broadcast delivery.


## Implementation validation (2026-09-14)

Pinned Rust 1.97.1: formatting, workspace Clippy with warnings denied, all 6,348
workspace tests (four existing skips), and workspace doctests passed. Coverage was
checked individually: networking 88.25%, core 92.93%, API 85.94%, model 88.42%.
Helm lint/template checks passed with discovery both enabled and disabled.

Live local server probes verified the four JSON keys and null endpoint, persisted
ID/name equality with public info, explicit bind, wildcard bind with LAN-address
selection, published URL, base-path reachability including repeated trailing
slashes, disabled discovery and nonfatal port conflict. Restart/rebind and changing
AutoDiscovery for the next lifetime are covered by real-socket integration tests.
The LAN-address probe ran on the server host; it is not separate-client broadcast
acceptance.

A local HTTP microbenchmark used the baseline and changed binaries built with the
same pinned compiler, unoptimized profile without debug symbols, no incremental
compilation, fresh data directories and persistent connections. Each of three
rounds had 20 warm-up requests and 300 measured `/System/Info/Public` requests.
Median round means were 0.217 ms before and 0.717 ms after. Live interface refresh
adds roughly 0.5 ms here; a configured published URL bypasses enumeration and
measured 0.218 ms. These measurements are diagnostic, not release throughput claims.

Remaining acceptance checks: a separate LAN client's broadcast and Jellyfin client,
live comparison with Jellyfin, native Windows/macOS interface transitions, and
Docker image build/smoke. Docker access on the validation host was denied and sudo
required a password; no .NET SDK or built upstream server was available. See
[the adapter decision](NETWORK_INTERFACE_ADAPTER.md) for platform compile checks and
the documented Linux ethtool difference.
