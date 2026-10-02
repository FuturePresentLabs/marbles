ARG BOB_IMAGE=registry.fpl.dev/fpl/bob@sha256:e2218f1381cc5651263f3385d4ec1249c585acb04d1bb92167f6397f0a4799d8

FROM ${BOB_IMAGE} AS bob

# Keep rustc byte-identical to Bob and the Fab runner. sccache keys include the
# exact compiler version, so a different patch silently turns every build cold.
FROM rust:1.98-slim-bookworm AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends make perl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=bob /usr/local/bin/sccache /usr/local/bin/sccache
WORKDIR /src
ARG SCCACHE_BUCKET=fpl-sccache-cache
ARG SCCACHE_ENDPOINT=https://919a14daf9d20924903a85f2da1df951.r2.cloudflarestorage.com
ARG SCCACHE_REGION=auto
ARG SCCACHE_S3_KEY_PREFIX=rust/
ENV CARGO_BUILD_JOBS=1 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_DEBUG=0 \
    RUSTC_WRAPPER=/usr/local/bin/sccache \
    SCCACHE_BUCKET=${SCCACHE_BUCKET} \
    SCCACHE_ENDPOINT=${SCCACHE_ENDPOINT} \
    SCCACHE_REGION=${SCCACHE_REGION} \
    SCCACHE_S3_KEY_PREFIX=${SCCACHE_S3_KEY_PREFIX} \
    SCCACHE_S3_USE_SSL=true \
    SCCACHE_BASEDIRS=/src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
# Integration tests reference both production binaries through CARGO_BIN_EXE_*,
# so this single Cargo invocation tests and emits the exact release artifacts
# copied into the runtime image. Do not add a second `cargo build` graph here.
RUN --mount=type=secret,id=AWS_ACCESS_KEY_ID \
    --mount=type=secret,id=AWS_SECRET_ACCESS_KEY \
    export AWS_ACCESS_KEY_ID="$(cat /run/secrets/AWS_ACCESS_KEY_ID)" \
           AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/AWS_SECRET_ACCESS_KEY)" \
 && sccache --zero-stats \
 && cargo test --locked --release --all-targets --all-features \
 && sccache --show-stats

FROM debian:12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /data --shell /usr/sbin/nologin marbles \
 && install -d -o marbles -g marbles -m 0700 /data/companies /data/tokens
COPY --from=builder /src/target/release/marbles /usr/local/bin/marbles
COPY --from=builder /src/target/release/marbles-pg-migrate /usr/local/bin/marbles-pg-migrate
COPY --chown=marbles:marbles deploy/server.toml /data/server.toml
USER marbles
ENV HOME=/data MARBLES_HOME=/data
EXPOSE 7878
ENTRYPOINT ["/usr/local/bin/marbles"]
CMD ["serve"]
