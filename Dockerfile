# syntax=docker/dockerfile:1
# omp-ntfy — same server binary as the native build, stateless container.
# The binary is built statically (x86_64-unknown-linux-musl) so the runtime
# image's libc never matters.
FROM rust:1.97 AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY ext ./ext
RUN cargo build --release --locked --target x86_64-unknown-linux-musl

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/omp-ntfy /usr/local/bin/omp-ntfy
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]