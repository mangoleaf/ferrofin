# Release image of the Hermit server:
#   - the hermit-server binary (built from this workspace),
#   - ffmpeg/ffprobe (scan probes with ffprobe, transcode needs ffmpeg),
#   - jellyfin-web (served at /web), pulled PREBUILT from the web CI image so this
#     build never runs npm/webpack.
#
# jellyfin-web is built once by the `web-image` CI job (ci/web.Dockerfile) and
# published to $CI_REGISTRY_IMAGE/ci:web-<JELLYFIN_WEB_VERSION>. WEB_IMAGE points
# at that tag; CI passes it via --build-arg (kept in sync with the pinned
# JELLYFIN_WEB_VERSION variable), and the default here matches for local builds.

# ── jellyfin-web (prebuilt — no npm/webpack here) ────────────────────
ARG WEB_IMAGE=registry.mangoleafstudios.com/mlstudios/hermit/ci:web-10.11.8
FROM ${WEB_IMAGE} AS web

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
