# Runtime base image — debian-slim + jellyfin-ffmpeg7 and the runtime libs the
# server needs (ffprobe for scan probes, ffmpeg for transcode, chromaprint for
# fingerprinting). Baked ONCE so the service image (root Dockerfile) doesn't
# re-run this ~150-package apt install on every build; it just `FROM`s this.
# Built + pushed by the `runtime-image` CI job to $CI_REGISTRY_IMAGE/ci:runtime-bookworm-ffmpeg7,
# rebuilt only when this file changes (else on demand via REBUILD_RUNTIME_IMAGE).
# Bump the tag (ci:runtime-<...>) when the ffmpeg major or the base distro moves.
FROM debian:bookworm-slim
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
