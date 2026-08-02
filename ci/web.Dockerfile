# jellyfin-web prep image — builds the pinned jellyfin-web ONCE so the service
# image (root Dockerfile) doesn't run npm/webpack on every build; it just
# `COPY --from` this image's /dist. Built + pushed by the `web-image` CI job to
# $CI_REGISTRY_IMAGE/ci:web-<JELLYFIN_WEB_VERSION>, rebuilt only when this file
# or the version changes (else on demand via REBUILD_WEB_IMAGE).
#
# The final image is FROM scratch and carries only /dist (the built static
# client), so it's a tiny COPY source (~tens of MB, no node/toolchain).
ARG JELLYFIN_WEB_VERSION=10.11.8
FROM node:20-bookworm AS build
ARG JELLYFIN_WEB_VERSION
# webpack's production build is memory-hungry; give Node headroom so CI doesn't OOM.
ENV NODE_OPTIONS=--max-old-space-size=4096
WORKDIR /web
RUN git clone --depth 1 --branch "v${JELLYFIN_WEB_VERSION}" \
      https://github.com/jellyfin/jellyfin-web.git . \
 && npm ci --no-audit --no-fund \
 && npm run build:production
# build:production emits the static client bundle to /web/dist.

FROM scratch
COPY --from=build /web/dist /dist
