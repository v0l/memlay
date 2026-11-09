FROM oven/bun:1 AS dashboard
WORKDIR /dash
COPY dashboard/package.json dashboard/bun.lock ./
RUN bun install --frozen-lockfile
COPY dashboard ./
RUN bun run build

FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY benches ./benches
COPY --from=dashboard /dash/dist/index.html dashboard/dist/index.html

RUN cargo build --release --bin memlay

FROM alpine:3

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/memlay /usr/local/bin/memlay

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/memlay"]
