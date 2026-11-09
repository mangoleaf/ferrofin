# Security Policy

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Email **ken@mangoleafstudios.com** with:

- a description of the issue and its impact,
- steps to reproduce (a proof of concept if you have one),
- affected version(s) / commit.

You'll get an acknowledgement within a few days. Once a fix is available and
released, we'll credit you in the release notes unless you prefer to stay
anonymous.

Ferrofin speaks the Jellyfin HTTP API and handles authentication, media, and
transcoding — please treat auth bypass, path traversal, SSRF, and unauthenticated
data exposure as high priority.

## Supported versions

Only the **latest released version** receives security fixes. Once maintenance
branches (`release-x.y`) exist, this section will list the supported window.

## Checking a binary for known vulnerabilities

From v1.5.1, every `ferrofin-server` binary a release ships carries the exact list of
crates it was built from, embedded by [cargo-auditable](https://github.com/rust-secure-code/cargo-auditable).
That covers the one in the container image, the one in the `.deb` and the ones in the
release archives. The release workflow checks each binary against the RustSec advisory
database and does not publish one with a known vulnerability that [`deny.toml`](deny.toml)
has not accepted (see below). To check an installed binary against advisories published
since:

```sh
cargo install --locked cargo-audit   # once; --max-binary-size needs 0.22.2 or later
B=/usr/bin/ferrofin-server           # the .deb's path; a tarball install's is /usr/local/bin
cargo audit bin --max-binary-size "$(stat -c %s "$B")" "$B"
```

cargo-audit reads at most 100 MiB of a binary by default, and Ferrofin's is just under
that, so the command raises the limit to the file's own size. The image's binary is
`/usr/local/bin/ferrofin-server`. Copy it out to check it:

```sh
docker create --name ferrofin-audit ghcr.io/mangoleaf/ferrofin:latest
docker cp ferrofin-audit:/usr/local/bin/ferrofin-server . && docker rm ferrofin-audit
cargo audit bin --max-binary-size "$(stat -c %s ferrofin-server)" ./ferrofin-server
```

An advisory in [`deny.toml`](deny.toml)'s `ignore` list is expected in the output:
unmaintained and unsound notices appear as warnings, a vulnerability as an error that
makes the command exit 1. The release check accepts those, each for the reason given
there. Container scanners that read cargo-auditable data (Trivy, Grype/Syft, osv-scanner)
find the same list in the image.
