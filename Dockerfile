FROM rust:1.97.1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY res ./res
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release --bin rs-luck-jingle \
    && cp /app/target/release/rs-luck-jingle /tmp/rs-luck-jingle

FROM builder AS test

COPY spec ./spec
COPY tests ./tests
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo test --locked --all-targets

FROM debian:bookworm-slim AS runtime

WORKDIR /app
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends bluez ca-certificates \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/rs-luck-jingle ./rs-luck-jingle
ENTRYPOINT ["./rs-luck-jingle"]
