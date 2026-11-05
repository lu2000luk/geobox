# Server image (default, last stage): docker build -t geobox-server .
# Generator image:                    docker build --target generator -t geobox-generator .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p geobox-server -p geobox-generator -p geobox-bench && mkdir /data-empty

# The generator runs `publish_command` through `sh`, so it gets a small Debian base;
# install your upload tool (aws-cli, rclone, uv for `uvx hf ...`) in a derived image.
FROM debian:bookworm-slim AS generator
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/geobox-generator /usr/local/bin/geobox-generator
WORKDIR /work
ENV GEOBOX_WORK_DIR=/work/work GEOBOX_OUTPUT_DIR=/work/data
ENTRYPOINT ["geobox-generator"]

# Load/quality benchmark: docker build --target bench -t geobox-bench .
FROM debian:bookworm-slim AS bench
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/geobox-bench /usr/local/bin/geobox-bench
ENTRYPOINT ["geobox-bench"]

FROM gcr.io/distroless/cc-debian12:nonroot AS server
COPY --from=build /src/target/release/geobox-server /usr/local/bin/geobox-server
# the image runs as uid 65532 ("nonroot"), so the data dir must belong to it
COPY --from=build --chown=65532:65532 /data-empty /data
# mimalloc: allocate from the OS, not arenas that stay committed (idle RAM; main.rs sets it too, but later)
ENV GEOBOX_DATA_DIR=/data GEOBOX_BIND=0.0.0.0:8080 MIMALLOC_DISALLOW_ARENA_ALLOC=1
VOLUME /data
EXPOSE 8080
# probes: GET /healthz (alive) and /readyz (initial sync done)
ENTRYPOINT ["/usr/local/bin/geobox-server"]
