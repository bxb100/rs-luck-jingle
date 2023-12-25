FROM rust:1.74.1-bullseye AS chef
WORKDIR /app
RUN cargo install cargo-chef

FROM chef AS planner
# Copy the whole project
COPY . .
# Prepare a build plan ("recipe")
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

# Copy the build plan from the previous Docker stage
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this layer is cached as long as `recipe.json`
# doesn't change.
RUN cargo chef cook --recipe-path recipe.json
COPY . .
# Build the project
RUN cargo build --release --bin rs-luck-jingle

# Runtime stage
FROM debian:bullseye-slim AS runtime
WORKDIR /app

COPY --from=builder /app/target/release/rs-luck-jingle rs-luck-jingle
ENTRYPOINT ["./rs-luck-jingle"]
