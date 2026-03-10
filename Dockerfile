FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

RUN cargo build --release --bin memlay

FROM alpine:3

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/memlay /usr/local/bin/memlay

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/memlay"]
