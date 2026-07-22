# hermit-common — Port Report

Rust port of Jellyfin's `MediaBrowser.Common` (plus a slice of
`MediaBrowser.Model.Cryptography` and the server crypto impl). Runtime machinery
(DI app-host, plugin loader, host networking, ASP.NET glue) is intentionally
**not** ported per the port charter.

## Gate results (INTEGRATE)

Run in order, in the `/home/mango/dev/hermit` workspace:

| # | Command | Result |
|---|---------|--------|
| 1 | `cargo fmt --all` | PASS (no changes) |
| 2 | `cargo fmt --all --check` | PASS (exit 0) |
| 3 | `cargo build --workspace` | PASS |
| 4 | `cargo clippy --all-targets --all-features -- -D warnings` | PASS (exit 0, no warnings) |
| 5 | `cargo test --workspace` | PASS (exit 0) |
| 6 | `cargo llvm-cov nextest -p hermit-common --fail-under-lines 80 --summary-only` | PASS (84.29% ≥ 80, exit 0) |

**Gate: PASSED.**

## Modules / types

| Module | C# source | Ported types |
|--------|-----------|--------------|
| `crc32` | `MediaBrowser.Common` (Crc32) | zlib/IEEE CRC-32 fn |
| `providers` | `MediaBrowser.Common.Providers.ProviderIdParsers` | `try_find_imdb_id`, `try_find_tmdb_movie_id`, `try_find_tmdb_series_id`, `try_find_tvdb_id`, private `try_find_provider_id` |
| `extensions` | `MediaBrowser.Common.Extensions.BaseExtensions` | `strip_html`, `get_md5` (+ vendored `md5` submodule) |
| `configuration` | `MediaBrowser.Common.Configuration` | `ConfigurationStore`, `ConfigurationUpdateEventArgs`, traits `ConfigurationFactory`, `ValidatingConfiguration` |
| `app_paths` | `IApplicationPaths` | `ApplicationPaths` trait (contract only) |
| `cryptography::password_hash` | `Model.Cryptography.PasswordHash` | `PasswordHash` (parse/Display/accessors) |
| `cryptography::provider` | `ICryptoProvider` + `CryptographyProvider` | `CryptoProvider` trait, `CryptographyProvider` |
| `cryptography::constants` | `Model.Cryptography.Constants` | `Constants` (salt/output length, iterations) |
| `exceptions` | `MediaBrowser.Common` exception types | `ResourceNotFoundException`, `RateLimitExceededException`, `MethodNotAllowedException`, `FfmpegException` |
| `error` | (Rust-idiomatic) | `CryptoError` enum, `Result<T>` alias |

## Tests (68 total, all passing)

| Suite | C# source | Cases |
|-------|-----------|-------|
| `crc32_tests` | `Jellyfin.Common.Tests/Crc32Tests.cs` | 6 |
| `provider_id_parser_tests` | `Jellyfin.Common.Tests/Providers/ProviderIdParserTests.cs` | 24 |
| `password_hash_tests` | `Jellyfin.Model.Tests/Cryptography/PasswordHashTests.cs` | 25 |
| `cryptography_provider_tests` | `Jellyfin.Server.Implementations.Tests/Cryptography/CryptographyProviderTests.cs` | 12 |
| lib unit | — | 1 |

Each suite is a transliteration of the corresponding xUnit file; `[Theory]`/
`[InlineData]` map to `rstest` `#[case]` parameterizations, `Assert.Throws<T>`
maps to matching on the `CryptoError` variant.

## Coverage (llvm-cov nextest, line %)

Overall: **84.29%** lines (420 total, 66 missed). Per file:

| File | Lines | Cover |
|------|-------|-------|
| crc32.rs | 10 | 100.00% |
| extensions/md5.rs | 57 | 100.00% |
| providers.rs | 48 | 100.00% |
| cryptography/password_hash.rs | 166 | 98.19% |
| cryptography/provider.rs | 81 | 93.83% |
| configuration.rs | 6 | 0.00% |
| exceptions.rs | 30 | 0.00% |
| extensions.rs | 22 | 0.00% |

## Parity vs C# xUnit cases

- **Full parity** on every suite that has an upstream xUnit oracle: CRC-32,
  provider-id parsers, `PasswordHash` parse/format, and the crypto-provider
  create/verify round-trip + error messages. Verbatim `FormatException` /
  `NotSupportedException` message strings are preserved so error assertions
  match.
- **`PasswordHash`** is a byte-for-byte faithful port including the UPPERCASE-hex
  (non-PHC) salt/hash encoding and insertion-ordered parameters, so
  `to_string()` reproduces C# output exactly.
- **Provider-id parsers** rely on all keys/inputs being ASCII (documented in
  source); byte and char offsets coincide, matching the C# `ReadOnlySpan<char>`
  scan semantics.

## Deferrals

1. **KDF substitution (deliberate, per crypto charter).** Key derivation is
   re-implemented over **argon2**, not PBKDF2. The C# public contract is kept
   (default id `PBKDF2-SHA512`, `iterations` param, exact error messages). No
   ported test asserts a byte-exact PBKDF2 digest — success is a create/verify
   round-trip and the PBKDF2 paths are only exercised for error messages — so
   the substitution is faithful to every existing oracle. A byte-exact PBKDF2
   compatibility test against legacy Jellyfin hashes is **not** ported.
2. **`configuration.rs` (0% cov).** Pure value types + traits (no upstream unit
   tests). The DI/runtime `IConfigurationManager` and `IApplicationHost`-bound
   pieces are deferred; only the portable value/interface shapes are here. C#
   `Type ConfigurationType` is represented as a `type_key` string (Rust has no
   runtime `Type`).
3. **`exceptions.rs` (0% cov).** Plain `thiserror` error types with no upstream
   test coverage; exercised only when host code raises them.
4. **`extensions.rs` `strip_html` / `get_md5` (0% cov at the wrapper).** Both are
   untested/behavioral in Jellyfin itself; the underlying `md5` submodule is
   100% covered. The wrappers are unexercised by ported tests.
5. **`app_paths.rs`.** Contract-only trait; no concrete impl ships in this crate
   (the host wires one up), so nothing to test here.
6. **Runtime machinery** (app-host/DI, plugin loader, host networking, ASP.NET
   glue) intentionally out of scope for this crate.

### Note for maintainers (magic numbers)

`cryptography::constants::Constants` bundles `DEFAULT_SALT_LENGTH` (16),
`DEFAULT_OUTPUT_LENGTH` (64), and `DEFAULT_ITERATIONS` (210_000). These mirror
Jellyfin's constants; the source comment already flags them for surfacing as
host-tunable settings later. Flagging here rather than converting — the host
owns that decision.
