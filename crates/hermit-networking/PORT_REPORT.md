---
type: PORT_REPORT
tags: [hermit, networking, port, integrate]
crate: hermit-networking
stage: INTEGRATE
last_verified: 2026-07-22T00:00Z
---

# Port Report — `hermit-networking`

Port of `Jellyfin.Networking` + the `MediaBrowser.Common.Net` address/subnet math.
Covers bind-address / published-URL resolution, RFC address ranges, CIDR/subnet
parsing, and the remote-access policy. This is the "First Light" target of the
Networking wave.

## Gate results (2026-07-22)

All six checks run in order, from `/home/mango/dev/hermit`:

| # | Check | Result |
|---|---|---|
| 1 | `cargo fmt --all` | PASS (exit 0) |
| 2 | `cargo fmt --all --check` | PASS (exit 0) |
| 3 | `cargo build --workspace` | PASS (exit 0) |
| 4 | `cargo clippy --all-targets --all-features -- -D warnings` | PASS (exit 0, no warnings) |
| 5 | `cargo test --workspace` | PASS (exit 0) |
| 6 | `cargo llvm-cov nextest -p hermit-networking --fail-under-lines 80 --summary-only` | PASS (exit 0) |

**Coverage: 87.75% lines (996/1135 covered), 90.76% functions, 87.63% regions.**
154 nextest cases run, 154 passed, 0 skipped. Gate floor is 80% → passed with a
~7.75-point margin.

Per-file line coverage:

| File | Lines | Cover | Notes |
|---|---|---|---|
| `logger.rs` | 1/1 | 100.00% | |
| `net_constants.rs` | 59/59 | 100.00% | |
| `network_configuration.rs` | 48/48 | 100.00% | |
| `net_utils.rs` | 334/361 | 92.52% | few parse-failure/edge paths uncovered, above gate |
| `manager.rs` | 554/666 | 83.18% | some error/edge branches in bind-interface resolution uncovered, above gate |
| **TOTAL** | **996/1135** | **87.75%** | |

## Types ported

Reuses `hermit_model::net` value types (`IpData`, `IpNetwork`, `AddressFamily`,
`PublishedServerUriOverride`) rather than redefining them. New types in this crate:

| Rust item | C# origin | Kind |
|---|---|---|
| `NetworkManager` | `Jellyfin.Networking.Manager.NetworkManager` | struct (resolver core) |
| `StartupConfig` | `IConfiguration` startup keys seam | struct |
| `NetworkConfiguration` | `MediaBrowser.Common.Net.NetworkConfiguration` | struct (settings DTO) |
| `RemoteAccessPolicyResult` | `MediaBrowser.Common.Net.RemoteAccessPolicyResult` | enum |
| `NetworkingError` | (Rust-native error type) | enum |
| `Logger` / `NullLogger` | `ILogger` seam | trait / struct |
| `net_constants::*` | `MediaBrowser.Common.Net.NetworkConstants` | consts + RFC-range fns |
| `net_utils::*` | `MediaBrowser.Common.Net.NetworkUtils` | free functions |
| `config_keys::*` | `MediaBrowser.Controller.Extensions.ConfigurationExtensions` | consts |

`NetworkManager` public surface: settings pipeline (`update_settings`,
`new`, `with_defaults`), and read-side queries (`get_bind_address`,
`get_bind_address_for_ip`, `get_all_bind_interfaces`, `get_internal_bind_addresses`,
`get_loopbacks`, `try_parse_interface`, `should_allow_server_access`,
`is_in_local_network` / `is_in_local_network_str`, `is_link_local_address`,
`is_ipv4_enabled`, `is_ipv6_enabled`, `trust_all_ipv6_interfaces`,
`published_server_urls`).

`net_utils` free functions: `is_ipv6_link_local`, `cidr_to_mask`, `mask_to_cidr`,
`format_ip_string`, `get_broadcast_address`, `subnet_contains_address`,
`try_parse_to_subnet`, `try_parse_to_subnets`, `try_parse_host`, `ip_none`.

## Tests (#154)

Split across five integration-test files plus in-crate unit tests. All are
ports of the Jellyfin xUnit suites (`rstest` `#[case(...)]` rows stand in for
`[InlineData]` / `[Theory]`):

| Test file | `#[test]`/`#[rstest]` fns | `#[case]` rows | C# xUnit origin |
|---|---|---|---|
| `network_parse_tests.rs` | 18 | 118 | `NetworkParseTests` |
| `network_extensions_tests.rs` | 4 | 31 | `NetworkExtensionsTests` |
| `network_manager_tests.rs` | 2 | 13 | `NetworkManagerTests` (bind resolution) |
| `manager_queries_tests.rs` | 10 | 0 | `NetworkManagerTests` (query side) |
| `network_configuration_tests.rs` | 1 | 10 | `NetworkConfigurationTests` |

nextest expands the case rows into individual test cases → **154 executed
cases** (the `--summary-only` run reports `154 tests run: 154 passed`).

## Parity vs the C# xUnit cases

The ported test files mirror the four upstream networking test classes
(`NetworkParseTests`, `NetworkExtensionsTests`, `NetworkManagerTests`,
`NetworkConfigurationTests`). Every case in those classes was ported EXCEPT the
deferred-subsystem cases below. Case-count parity is high: the parse/extension/
configuration classes are ported row-for-row; the manager class is ported for
its deterministic settings-pipeline + read-query cases and defers only the cases
that exercise OS eventing and live interface enumeration (which have no Rust
analogue yet — see below).

One environment-conditional case is retained faithfully: `network_parse_tests.rs`
skips a DNS-resolution assertion at runtime when the host has no working DNS
(same guard the upstream test uses), so it is deterministic in CI.

## Deferrals

Documented in-crate (`lib.rs`, `manager.rs`, `config_keys.rs` doc comments) and
in the workspace ledger `brain/DEFERRED.md`. Nothing silently dropped:

- **Live interface enumeration** (`GetInterfacesCore`) — deferred. With no mock
  interface string the interface list starts empty; real hosts inject interfaces
  once a platform adapter lands. This is the main driver of the uncovered
  `manager.rs` branches (some bind-interface resolution paths only fire with a
  populated live interface set).
- **OS `NetworkChange` event wiring** + the `Thread.Sleep(2000)` debounce +
  `Dispose` — deferred (OS eventing). The `DetectNetworkChange` config key is
  still parsed/stored so clients don't 404; only the eventing machinery is stubbed.
- **UDP `AutoDiscoveryHost`, `SocketFactory`** — deferred.
- **Happy-Eyeballs `HttpClientExtension`** — not ported; `reqwest` performs
  Happy-Eyeballs itself, so there's no Rust analogue to port.
- **`HttpRequest` overload of the bind resolver** — deferred (framework-specific).

### Uncovered lines (both above the 80% gate)

- `manager.rs` 554/666 (83.18%): remaining uncovered lines are error/edge
  branches in bind-interface resolution that only trigger with live-enumerated
  interfaces (deferred) or malformed operator config.
- `net_utils.rs` 334/361 (92.52%): a handful of parse-failure / off-polarity
  edge paths in `try_parse_to_subnet(s)` / `try_parse_host`.

Un-defer path for the coverage residue: once the live-interface adapter lands,
the currently-unreachable `manager.rs` bind branches become testable and coverage
should rise further.
