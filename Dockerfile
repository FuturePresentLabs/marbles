FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo test --locked --all-targets --all-features \
 && cargo build --locked --release

FROM debian:12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /data --shell /usr/sbin/nologin marbles \
 && install -d -o marbles -g marbles -m 0700 /data/companies /data/tokens
COPY --from=builder /src/target/release/marbles /usr/local/bin/marbles
COPY --chown=marbles:marbles deploy/server.toml /data/server.toml
USER marbles
ENV HOME=/data MARBLES_HOME=/data
EXPOSE 7878
ENTRYPOINT ["/usr/local/bin/marbles"]
CMD ["serve"]
