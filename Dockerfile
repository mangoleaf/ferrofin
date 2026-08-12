# Release image of the Hermit server:
#   - the hermit-server binary (built from this workspace),
#   - ffmpeg/ffprobe (scan probes with ffprobe, transcode needs ffmpeg),
#   - jellyfin-web (served at /web).
#
# A plain `docker build .` is fully self-contained: the `web-build` and
# `runtime-build` stages below build jellyfin-web and the ffmpeg runtime from
# public sources. CI keeps its fast path by overriding WEB_IMAGE/RUNTIME_IMAGE
# with prebuilt base images (ci/web.Dockerfile, ci/runtime.Dockerfile — same
# contents, baked once); BuildKit then skips the unused local stages entirely.
ARG WEB_IMAGE=web-build
ARG RUNTIME_IMAGE=runtime-build

# ── jellyfin-web, built from source (skipped when CI passes WEB_IMAGE) ──
ARG JELLYFIN_WEB_VERSION=10.11.8
FROM node:20-bookworm AS web-source
ARG JELLYFIN_WEB_VERSION
# webpack's production build is memory-hungry; give Node headroom.
ENV NODE_OPTIONS=--max-old-space-size=4096
WORKDIR /web
RUN git clone --depth 1 --branch "v${JELLYFIN_WEB_VERSION}" \
      https://github.com/jellyfin/jellyfin-web.git . \
 && npm ci --no-audit --no-fund \
 && npm run build:production
FROM scratch AS web-build
COPY --from=web-source /web/dist /dist

# ── ffmpeg runtime base (skipped when CI passes RUNTIME_IMAGE) ──────────
# jellyfin-ffmpeg over Debian's ffmpeg: SIMD single-pass tonemapping and a
# current libx264 — the difference on 4K HDR transcode start times.
FROM debian:bookworm-slim AS runtime-build
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg libchromaprint-tools \
 && curl -fsSL https://repo.jellyfin.org/jellyfin_team.gpg.key \
      | gpg --dearmor -o /usr/share/keyrings/jellyfin.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/jellyfin.gpg] https://repo.jellyfin.org/debian bookworm main" \
      > /etc/apt/sources.list.d/jellyfin.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends jellyfin-ffmpeg7 \
 && ln -s /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/local/bin/ffmpeg \
 && ln -s /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ffprobe \
 && rm -rf /var/lib/apt/lists/*

# ── prebuilt-or-local indirection ───────────────────────────────────────
FROM ${WEB_IMAGE} AS web

# ── server binary ───────────────────────────────────────────────────────
FROM rust:1.97.1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p hermit-server

# ── runtime ─────────────────────────────────────────────────────────────
FROM ${RUNTIME_IMAGE}
COPY --from=build /src/target/release/hermit-server /usr/local/bin/hermit-server
COPY --from=web /dist /usr/share/hermit/web
# The release version, passed from CI (--build-arg SERVICE_VERSION=<tag>). The
# binary reports it (falling back to the crate version when unset for local builds).
ARG SERVICE_VERSION=
ENV SERVICE_VERSION=$SERVICE_VERSION
# HERMIT_WEB_DIR lives OUTSIDE /data on purpose: /data is a mounted volume that
# would shadow anything baked under it.
ENV HERMIT_DATA_DIR=/data HERMIT_BIND_ADDR=0.0.0.0 HERMIT_PORT=8096 \
    HERMIT_WEB_DIR=/usr/share/hermit/web
VOLUME /data
EXPOSE 8096
ENTRYPOINT ["hermit-server"]
