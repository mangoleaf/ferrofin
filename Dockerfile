# Release image of the Hermit server (ffmpeg included: scan probes with ffprobe,
# transcode needs ffmpeg). Same recipe as benchmark/Dockerfile.hermit, kept at the
# root as the canonical deployment image built by CI.
FROM rust:1.97.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p hermit-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/hermit-server /usr/local/bin/hermit-server
ENV HERMIT_DATA_DIR=/data HERMIT_BIND_ADDR=0.0.0.0 HERMIT_PORT=8096
VOLUME /data
EXPOSE 8096
ENTRYPOINT ["hermit-server"]
