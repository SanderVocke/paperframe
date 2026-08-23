# syntax=docker/dockerfile:1

FROM rust:1.85-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 paperframe \
    && useradd --uid 10001 --gid paperframe --no-create-home --shell /usr/sbin/nologin paperframe \
    && mkdir -p /config /data/queue /photos /usr/share/paperframe \
    && chown -R paperframe:paperframe /config /data /photos /usr/share/paperframe

COPY --from=builder /build/target/release/paperframe-server /usr/local/bin/paperframe-server
COPY --chmod=0755 docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY --chown=paperframe:paperframe docker/config.toml /config/config.toml
COPY --chown=paperframe:paperframe docker/settings.toml /usr/share/paperframe/settings.toml

LABEL org.opencontainers.image.title="Paper Frame Server" \
      org.opencontainers.image.description="A photo queue and image server for PaperlessPaper frames"

USER 10001:10001
VOLUME ["/data"]
EXPOSE 8080
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:8080/health/live"]

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["/config/config.toml"]
