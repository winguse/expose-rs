# ── Dependency cache layer (cargo-chef) ──────────────────────────────────────
FROM rust:slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release -p expose-server -p expose-client

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/expose-server /usr/local/bin/expose-server
COPY --from=builder /src/target/release/expose-client /usr/local/bin/expose-client

# Default to the server binary; override with expose-client when using as a client.
CMD ["expose-server"]
