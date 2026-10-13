# The RagMonk image. Built from the repository root:
#
#   docker build --build-arg RAGMONK_VERSION=1.2.0 -t ragmonk .
#
# Multi-arch images are built natively per platform (amd64 and arm64
# runners) by .github/workflows/rust-release.yml and joined into one
# manifest; nothing here cross-compiles.

# ---------------------------------------------------------------- build ---
FROM rust:1.90.0-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ARG RAGMONK_VERSION=""
ARG RAGMONK_COMMIT=""
ARG RAGMONK_MINISIGN_PUBKEY=""
ENV RAGMONK_BUILD_VERSION=${RAGMONK_VERSION} \
    RAGMONK_BUILD_COMMIT=${RAGMONK_COMMIT} \
    RAGMONK_MINISIGN_PUBKEY=${RAGMONK_MINISIGN_PUBKEY} \
    CARGO_INCREMENTAL=0
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p docmonk-cli \
    && cp target/release/ragmonk /usr/local/bin/ragmonk
RUN sh scripts/fetch_models.sh /opt/ragmonk/models

# -------------------------------------------------------------- runtime ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin ragmonk \
    && mkdir -p /data && chown ragmonk:ragmonk /data
COPY --from=build /usr/local/bin/ragmonk /usr/local/bin/ragmonk
COPY --from=build /opt/ragmonk/models /opt/ragmonk/models
# The image is updated by pulling a new tag, never by `ragmonk update`.
ENV RAGMONK_HOME=/data \
    RAGMONK_MODELS_DIR=/opt/ragmonk/models \
    RAGMONK_OCR_MODELS_DIR=/opt/ragmonk/models/ocrs \
    RAGMONK_NO_UPDATE_CHECK=1
USER ragmonk
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8765
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ragmonk"]
CMD ["--help"]
