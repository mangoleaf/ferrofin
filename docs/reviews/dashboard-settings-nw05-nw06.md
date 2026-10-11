# NW05 / NW06: the listener follows the network settings

The dashboard's **Local IP address** (`LocalNetworkAddresses`) and **Enable
IPv4 / IPv6** (`EnableIPv4`, `EnableIPv6`) settings changed which interfaces
Ferrofin advertised and which clients counted as local, but the HTTP listener
always bound the single startup address (`0.0.0.0` by default). Restricting
the server to one interface, or turning a family off, did nothing to what it
accepted.

Source oracle: Jellyfin `4910aafa1a`, `WebHostBuilderExtensions.
SetupJellyfinWebServer` (one `Listen` per address of
`NetManager.GetAllBindInterfaces(false)`) and `NetworkManager.
GetAllBindInterfaces`. Ferrofin's port of the latter already existed and was
unused by the listener.

## Behavior

- **No startup bind given** (no `--bind`, `FERROFIN_BIND_ADDR` or
  `config.toml` `bind_addr`): the listeners are `GetAllBindInterfaces(false)`
  over the saved network settings:
  - `LocalNetworkAddresses` set → one listener per resolved interface address;
  - otherwise IPv4 only → `0.0.0.0`; IPv4 and IPv6 → a dual-stack `::`;
    IPv6 only → the host's IPv6 interface addresses, link-local ones bound on
    their interface (the scope id a .NET `IPAddress` carries);
  - both families off → startup fails with a message saying so (Kestrel with no
    `Listen` has nothing to serve).
- **A startup bind given** stays the one listener. This is a deliberate
  divergence: Jellyfin has no such flag, and an address an operator pins in a
  container or service file is an explicit exposure choice that a dashboard
  change should not widen. The default is unchanged (`0.0.0.0`, like Jellyfin's
  all-interfaces default).
- `::` is bound dual-stack explicitly (`IPV6_V6ONLY` off) rather than by
  platform default, which is IPv6-only on Windows and the BSDs. A host without
  IPv6 falls back to `0.0.0.0` with a warning, as Jellyfin drops IPv6 when the
  OS does not support it.
- Several listeners share one router, one drain trigger and one shutdown
  timeout; the first one names the bound port reported to the host. Changing the
  settings takes effect at the next start, as in Jellyfin.

## Verification

- Unit (`listen`): an operator's bind wins over the manager's list; a list
  becomes one address per interface on the port, link-local IPv6 with its scope;
  several binds yield several listeners; `::` accepts an IPv4 client; an address
  in use is an error naming it. The nagle test, which binds through the same
  function, still passes.
- Production binary, previous baseline vs updated, started without `--bind` on
  a host with a LAN address, IPv6 on `lo`, and `network.json` varied. Reachable
  = a TCP connect succeeds:

  | network settings | before: 127.0.0.1 / ::1 / LAN | after: 127.0.0.1 / ::1 / LAN |
  |---|---|---|
  | defaults | yes / no / yes | yes / no / yes |
  | `LocalNetworkAddresses = [127.0.0.1]` | yes / no / **yes** | yes / no / **no** |
  | `EnableIPv6 = true` | yes / **no** / yes | yes / **yes** / yes |
  | `EnableIPv4 = false, EnableIPv6 = true` | **yes** / no / **yes** | **no** / yes / **no** |
  | both off | boots | **refuses to start**, naming why |
  | `--bind 127.0.0.1` with `LocalNetworkAddresses = [LAN]` | yes / no / no | yes / no / no |

## Limitations

- The HTTPS ports and certificate (NW01–NW02) and the base URL (NW03) are
  separate findings and unchanged.
- Interface addresses are resolved when the server starts; a changed address
  needs a restart, as in Jellyfin.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for the server
package passed: server 235. Real FFmpeg tests enabled; the WASM guest build
remains disabled. Clippy flagged an over-long `serve_once`, an unneeded `async`
and two style lints in new code; they were fixed (the bind moved into a helper)
and Clippy, the listener tests and the live check were rerun clean, while the
full package run and doctests were taken before those edits. The server is
exempt from the coverage gate (composition root); the planner and listener
logic it contains is tested above. The dependency added is `socket2` 0.6, the
latest release and already in the lockfile through tokio.
