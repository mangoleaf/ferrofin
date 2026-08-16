# Release image of the Ferrofin server:
#   - the ferrofin-server binary (built from this workspace),
#   - ffmpeg/ffprobe (scan probes with ffprobe, transcode needs ffmpeg),
#   - jellyfin-web (served at /web).
#
# A plain `docker build .` is fully self-contained: the `web-build` and
# `runtime-build` stages below build jellyfin-web and the ffmpeg runtime from
# public sources. CI keeps its fast path by overriding WEB_IMAGE/RUNTIME_IMAGE
# with prebuilt base images (ci/web.Dockerfile, ci/runtime.Dockerfile — same
# contents, baked once); BuildKit then skips the unused local stages entirely.
# NOTE: kaniko does NOT skip unused stages by default — the CI service-image
# build MUST pass `--skip-unused-stages=true` or it rebuilds jellyfin-web from
# source every release (see .gitlab-ci.yml).
ARG WEB_IMAGE=web-build
ARG RUNTIME_IMAGE=runtime-build

# ── jellyfin-web, built from source (skipped when CI passes WEB_IMAGE) ──
ARG JELLYFIN_WEB_VERSION=10.11.8
FROM node:20-bookworm AS web-source
ARG JELLYFIN_WEB_VERSION
# webpack's production build is memory-hungry; give Node headroom.
ENV NODE_OPTIONS=--max-old-space-size=4096
WORKDIR /web
# jellyfin-web pulls a couple of deps from github.com archive URLs that ECONNRESET;
# raise npm's fetch retries and retry `npm ci` so a transient blip doesn't fail the build.
RUN git clone --depth 1 --branch "v${JELLYFIN_WEB_VERSION}" \
      https://github.com/jellyfin/jellyfin-web.git . \
 && npm config set fetch-retries 5 fetch-retry-mintimeout 20000 fetch-retry-maxtimeout 120000 \
 && n=0; until npm ci --no-audit --no-fund; do \
      n=$((n+1)); [ "$n" -ge 5 ] && echo "npm ci failed after $n attempts" && exit 1; \
      echo "npm ci failed, retry $n/5 after 15s"; sleep 15; \
    done \
 && npm run build:production
FROM scratch AS web-build
COPY --from=web-source /web/dist /dist

# ── ffmpeg runtime base (skipped when CI passes RUNTIME_IMAGE) ──────────
# jellyfin-ffmpeg over Debian's ffmpeg: SIMD single-pass tonemapping and a
# current libx264 — the difference on 4K HDR transcode start times. It is also
# built --enable-chromaprint, which is what the intro skipper fingerprints
# with; bookworm's libchromaprint-tools (fpcalc 1.5.1, a 2020 release) is
# deliberately NOT installed — it aborts any window that decodes to
# end-of-stream, which is every credits window.
FROM debian:bookworm-slim AS runtime-build
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
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
RUN cargo build --release -p ferrofin-server

# ── runtime ─────────────────────────────────────────────────────────────
FROM ${RUNTIME_IMAGE}
COPY --from=build /src/target/release/ferrofin-server /usr/local/bin/ferrofin-server
COPY --from=web /dist /usr/share/ferrofin/web
# The release version, passed from CI (--build-arg SERVICE_VERSION=<tag>). The
# binary reports it (falling back to the crate version when unset for local builds).
ARG SERVICE_VERSION=
ENV SERVICE_VERSION=$SERVICE_VERSION
# FERROFIN_WEB_DIR lives OUTSIDE /data on purpose: /data is a mounted volume that
# would shadow anything baked under it.
ENV FERROFIN_DATA_DIR=/data FERROFIN_BIND_ADDR=0.0.0.0 FERROFIN_PORT=8096 \
    FERROFIN_WEB_DIR=/usr/share/ferrofin/web
# Run as a fixed non-root UID (1000, the conventional first-user id media
# containers standardize on). /data is chowned BEFORE the VOLUME declaration so
# anonymous volumes inherit the ownership; bind mounts are the operator's job —
# chown them to 1000:1000 or override with `docker run --user`.
RUN useradd --uid 1000 --user-group --home-dir /data --no-create-home \
      --shell /usr/sbin/nologin ferrofin \
 && mkdir -p /data && chown ferrofin:ferrofin /data
USER ferrofin
VOLUME /data
EXPOSE 8096
ENTRYPOINT ["ferrofin-server"]
