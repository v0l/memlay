FROM oven/bun:1 AS dashboard
WORKDIR /dash
COPY dashboard/package.json dashboard/bun.lock ./
RUN bun install --frozen-lockfile
COPY dashboard ./
RUN bun run build

FROM rust:1-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY benches ./benches
COPY --from=dashboard /dash/dist/index.html dashboard/dist/index.html

RUN cargo build --release --bin memlay

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/memlay /usr/local/bin/memlay

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/memlay"]
