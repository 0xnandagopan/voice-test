# syntax=docker/dockerfile:1
FROM rust:1.98.0-bookworm AS build
# Native crypto dependencies in the S3 client require a C toolchain and CMake.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY migrations ./migrations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p v0-app -p v0-worker

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 voice-test \
    && useradd --uid 10001 --gid voice-test --no-create-home --shell /usr/sbin/nologin voice-test \
    && mkdir -p /app /tmp/voice-test-jobs \
    && chown voice-test:voice-test /app /tmp/voice-test-jobs
COPY --from=build /src/target/release/v0-app /usr/local/bin/v0-app
COPY --from=build /src/target/release/v0-worker /usr/local/bin/v0-worker
WORKDIR /app
USER 10001:10001
ENV SERVE_WEB=false \
    EVIDENCE_STORAGE_BACKEND=s3 \
    EVIDENCE_WORK_DIR=/tmp/voice-test-jobs \
    FFMPEG_PATH=/usr/bin/ffmpeg \
    FFPROBE_PATH=/usr/bin/ffprobe \
    RUST_LOG=info
# Railway supplies PORT. Local use: docker run -e PORT=3000 -p 3000:3000 ...
# The worker service overrides this command with /usr/local/bin/v0-worker.
CMD ["/usr/local/bin/v0-app"]
