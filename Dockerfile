# Release image of the Hermit server. Bundles three things:
#   - the hermit-server binary (built from this workspace),
#   - ffmpeg/ffprobe (scan probes with ffprobe, transcode needs ffmpeg),
#   - the jellyfin-web client, built from a PINNED tag and served at /web.
#
# jellyfin-web is NOT vendored in this repo — it is cloned + built here so the
# image is self-contained and a browser at / loads the UI. The version is pinned
# (JELLYFIN_WEB_VERSION) so an upstream web release can't silently break the
# image; bump it deliberately, matching the Jellyfin API version Hermit reports.

# ── jellyfin-web ───────────────────────────────────────────────────
# Pinned to the Jellyfin API version Hermit advertises. Override at build time
# with --build-arg JELLYFIN_WEB_VERSION=<x.y.z>; change the default to bump.
ARG JELLYFIN_WEB_VERSION=10.11.8
FROM node:20-bookworm AS web
ARG JELLYFIN_WEB_VERSION
# webpack's production build is memory-hungry; give Node headroom so CI doesn't OOM.
ENV NODE_OPTIONS=--max-old-space-size=4096
WORKDIR /web
RUN git clone --depth 1 --branch "v${JELLYFIN_WEB_VERSION}" \
      https://github.com/jellyfin/jellyfin-web.git . \
 && npm ci --no-audit --no-fund \
 && npm run build:production
# build:production emits the static client bundle to /web/dist.

# ── server binary ──────────────────────────────────────────────────
FROM rust:1.97.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p hermit-server

# ── runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/hermit-server /usr/local/bin/hermit-server
COPY --from=web /web/dist /usr/share/hermit/web
# HERMIT_WEB_DIR lives OUTSIDE /data on purpose: /data is a mounted volume that
# would shadow anything baked under it.
ENV HERMIT_DATA_DIR=/data HERMIT_BIND_ADDR=0.0.0.0 HERMIT_PORT=8096 \
    HERMIT_WEB_DIR=/usr/share/hermit/web
VOLUME /data
EXPOSE 8096
ENTRYPOINT ["hermit-server"]
