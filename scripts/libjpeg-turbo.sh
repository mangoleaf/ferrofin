#!/usr/bin/env bash
# Builds the libjpeg-turbo every build of Ferrofin links, statically: the official release pinned
# below by version and SHA-256, downloaded, checked and compiled with cmake (and nasm on x86-64,
# whose SIMD code REQUIRE_SIMD makes mandatory). turbojpeg-sys carries an older libjpeg-turbo and
# is built without it (Cargo.toml): it links the library in TURBOJPEG_LIB_DIR, which
# .cargo/config.toml points at target/libjpeg-turbo-<version>/lib, where this script installs.
# The directory is named after the version, so a checkout never links another pin's library.
#
# Run it once per checkout before the first cargo build, and again when the pin moves or target/
# is deleted; it does nothing while the directory holds this build.
#
#   scripts/libjpeg-turbo.sh [--zig-target <zig target>]
#   scripts/libjpeg-turbo.sh --check <binary>
#
# --zig-target x86_64-linux-gnu.2.34 compiles with `python3 -m ziglang cc -target ...`, the
# compiler cargo-zigbuild gives the Rust build in release.yml, so the library needs no newer
# glibc than the binary may. --check builds nothing: it fails unless the binary links the pinned
# version, which the release builds ask of what they ship.
#
# Moving the pin: take SHA256 from the release's own asset list
# (`gh api repos/libjpeg-turbo/libjpeg-turbo/releases/tags/<version>`, the digest of
# libjpeg-turbo-<version>.tar.gz), check the tarball's signature (the .sig beside it) against the
# project's signing key, and change the directory name in .cargo/config.toml with VERSION.
# docs/SUPPLY_CHAIN.md records what was checked for the current pin.
set -euo pipefail

VERSION=3.2.1
SHA256=c27ad72a68da151177929aa969ae773a2da5ac2af9ce946079314fdac8cece7f
# The release's LICENSE.md. about.hbs reproduces its Modified BSD License notice in the notices
# every artifact ships: when this changes, read the file again and update the notice there.
LICENSE_SHA256=ba6bceebcba0fdd35488477c2cca8c4632ce82c74dbfbc87d886ce6fc4433579

die() {
  echo "libjpeg-turbo.sh: $*" >&2
  exit 1
}

zig_target=
while [ $# -gt 0 ]; do
  case $1 in
    --zig-target)
      [ $# -ge 2 ] || die "--zig-target needs a zig target, such as x86_64-linux-gnu.2.34"
      zig_target=$2
      shift 2
      ;;
    --check)
      [ $# -ge 2 ] || die "--check needs a binary"
      # libjpeg-turbo's compressor (jcmaster.c) carries the version as a string, so that a binary
      # linking it can be told from one linking another version.
      grep -aqF "libjpeg-turbo version $VERSION " "$2" \
        || die "$2 does not link libjpeg-turbo $VERSION"
      exit 0
      ;;
    *) die "usage: scripts/libjpeg-turbo.sh [--zig-target <zig target>] | --check <binary>" ;;
  esac
done

prefix="$(cd "$(dirname "$0")/.." && pwd)/target/libjpeg-turbo-$VERSION"
stamp="$VERSION $SHA256 ${zig_target:-native}"
if [ "$(cat "$prefix/stamp" 2>/dev/null)" = "$stamp" ]; then
  exit 0
fi

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# Nothing of an earlier build stays, so a build that fails here leaves no library to link.
# Everything below runs inside the work directory and names paths relative to it, so cmake gets
# no absolute path for Git Bash on Windows to rewrite.
rm -rf -- "${prefix:?}"
mkdir -p "$prefix/work"
cd "$prefix/work"

tarball=libjpeg-turbo-$VERSION.tar.gz
curl -fsSL --retry 5 --retry-all-errors -o "$tarball" \
  "https://github.com/libjpeg-turbo/libjpeg-turbo/releases/download/$VERSION/$tarball"
[ "$(sha256 "$tarball")" = "$SHA256" ] \
  || die "$tarball does not have the pinned SHA-256 ($SHA256): not building it"
tar -xzf "$tarball"
source=libjpeg-turbo-$VERSION
[ "$(sha256 "$source/LICENSE.md")" = "$LICENSE_SHA256" ] \
  || die "the release's LICENSE.md changed: update the notice in about.hbs, then LICENSE_SHA256"

# Static only, with position-independent code (Rust links position-independent executables),
# and without the command-line tools and their tests.
args=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_INSTALL_LIBDIR=lib
  -DCMAKE_POSITION_INDEPENDENT_CODE=ON
  -DENABLE_SHARED=OFF
  -DENABLE_STATIC=ON
  -DREQUIRE_SIMD=ON
  -DWITH_TOOLS=OFF
)
if [ -n "$zig_target" ]; then
  printf '#!/bin/sh\nexec python3 -m ziglang cc -target %s "$@"\n' "$zig_target" >zig-cc
  printf '#!/bin/sh\nexec python3 -m ziglang ar "$@"\n' >zig-ar
  printf '#!/bin/sh\nexec python3 -m ziglang ranlib "$@"\n' >zig-ranlib
  chmod +x zig-cc zig-ar zig-ranlib
  args+=(
    -DCMAKE_SYSTEM_NAME=Linux
    "-DCMAKE_SYSTEM_PROCESSOR=${zig_target%%-*}"
    "-DCMAKE_C_COMPILER=$PWD/zig-cc"
    "-DCMAKE_AR=$PWD/zig-ar"
    "-DCMAKE_RANLIB=$PWD/zig-ranlib"
  )
fi
case $(uname -s) in
  # Rust's MSVC targets link the C run-time DLL; libjpeg-turbo's default is the static one.
  MINGW* | MSYS*) args+=(-DWITH_CRT_DLL=ON) ;;
esac

cmake -S "$source" -B build "${args[@]}"
cmake --build build --config Release --parallel 4
cmake --install build --config Release --prefix ..
cd ..
# Visual C++ builds name the static library turbojpeg-static.lib; turbojpeg-sys links "turbojpeg".
if [ -f lib/turbojpeg-static.lib ]; then
  cp lib/turbojpeg-static.lib lib/turbojpeg.lib
fi
rm -rf -- "${prefix:?}/work"
echo "$stamp" >stamp
echo "libjpeg-turbo $VERSION installed in $prefix"
