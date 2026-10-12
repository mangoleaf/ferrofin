#!/bin/sh
# Writes the .deb's generated files into target/deb/ (see the nfpm contents in
# apps/ferrofin-server/Cargo.toml): the Debian changelog, rendered from git
# history by git-cliff, and the gzipped man page. Run from the repo root with
# FERROFIN_VERSION set, as for `cargo nfpm package`.
set -eu
mkdir -p target/deb
DEB_MAINTAINER=$(sed -n 's/^maintainer = "\(.*\)"$/\1/p' apps/ferrofin-server/Cargo.toml) \
    GIT_CLIFF__CHANGELOG__TRIM=false git-cliff --config cliff.toml --tag "v${FERROFIN_VERSION:?}" --strip all \
    --body "$(cat contrib/debian/changelog.tera)" | gzip -9n > target/deb/changelog.gz
gzip -9nc contrib/debian/ferrofin-server.1 > target/deb/ferrofin-server.1.gz
