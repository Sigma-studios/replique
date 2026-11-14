# Réplique's server, for façade or any Docker host.
#
#   docker build -t replique-server .
#   docker run -p 8080:8080 -v replique:/data \
#     -e REPLIQUE_PUBLIC_URL=https://replique.example.com \
#     -e REPLIQUE_ADMIN_PASSWORD_HASH='$argon2id$…' replique-server
#
# Everything the server keeps (database, master key, its SSH key, restic's
# cache) is in /data. Configuration comes from /data/replique.toml if present,
# and from REPLIQUE_* variables (docs/server.md lists them).

FROM rust:1.95-alpine AS build
# musl gives a static binary; a C compiler builds SQLite and ring.
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY proto proto
RUN cargo build --release --locked -p replique-server \
    && strip target/release/replique-server

FROM alpine:3.22
# restic serves website restores; it reaches the box through the server's own
# SSH (`replique-server sftp-bridge`), so no OpenSSH client is needed.
RUN apk add --no-cache restic ca-certificates tzdata \
    && adduser -D -H -u 10001 replique \
    && mkdir /data && chown replique /data
COPY --from=build /src/target/release/replique-server /usr/local/bin/replique-server
USER replique
ENV REPLIQUE_CONFIG=/data/replique.toml \
    REPLIQUE_LISTEN=0.0.0.0:8080 \
    REPLIQUE_STATE_DIR=/data
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=60s --timeout=5s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["replique-server"]
CMD ["serve"]
