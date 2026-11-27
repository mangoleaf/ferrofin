#!/bin/sh
# Writes target/THIRD-PARTY-NOTICES.txt, the licenses and copyright notices of the third-party code built
# into ferrofin-server, from Cargo.lock with cargo-about (about.toml, about.hbs). The image, the .deb and
# the release archives ship it, and CI's `dependencies` job runs this as their check. Run it from the
# repository root, with cargo-about installed at the version ci.yml pins.
#
# Offline, so the file depends only on Cargo.lock and about.toml: every target's crates are fetched
# first, and a crate that ships no license file gets the license's standard text instead of one fetched
# from its repository. cargo-about only warns when a clarification in about.toml no longer matches its
# file, or names a crate that has left the graph, and then guesses; any output from it fails this.
set -eu
cargo fetch --locked
mkdir -p target
rc=0
cargo about generate --frozen --fail -m apps/ferrofin-server/Cargo.toml -c about.toml \
    -o target/THIRD-PARTY-NOTICES.txt about.hbs 2> target/third-party-notices.log || rc=$?
if [ "$rc" -ne 0 ] || [ -s target/third-party-notices.log ]; then
    cat target/third-party-notices.log >&2
    rm -f target/THIRD-PARTY-NOTICES.txt
    echo "cargo-about failed or warned (exit $rc): fix about.toml before shipping the notices" >&2
    exit 1
fi
