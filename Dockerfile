# The DocMonk image. Built from the repository root:
#
#   docker build --build-arg DOCMONK_VERSION=1.2.0 -t docmonk .
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
ARG DOCMONK_VERSION=""
ARG DOCMONK_COMMIT=""
ARG DOCMONK_MINISIGN_PUBKEY=""
ENV DOCMONK_BUILD_VERSION=${DOCMONK_VERSION} \
    DOCMONK_BUILD_COMMIT=${DOCMONK_COMMIT} \
    DOCMONK_MINISIGN_PUBKEY=${DOCMONK_MINISIGN_PUBKEY} \
    CARGO_INCREMENTAL=0
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p docmonk-cli \
    && cp target/release/docmonk /usr/local/bin/docmonk
RUN sh scripts/fetch_models.sh /opt/docmonk/models

# -------------------------------------------------------------- runtime ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin docmonk \
    && mkdir -p /data && chown docmonk:docmonk /data
COPY --from=build /usr/local/bin/docmonk /usr/local/bin/docmonk
COPY --from=build /opt/docmonk/models /opt/docmonk/models
# The image is updated by pulling a new tag, never by `docmonk update`.
ENV DOCMONK_HOME=/data \
    DOCMONK_MODELS_DIR=/opt/docmonk/models \
    DOCMONK_OCR_MODELS_DIR=/opt/docmonk/models/ocrs \
    DOCMONK_NO_UPDATE_CHECK=1
USER docmonk
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8765
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docmonk"]
CMD ["--help"]
