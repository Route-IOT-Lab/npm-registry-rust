# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /build

# Compile dependencies first so source-only changes reuse the cached layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# `static/` and `migrations/` are pulled in at compile time by `include_str!`
# and `sqlx::migrate!`, so they must be present before the final build.
COPY src ./src
COPY static ./static
COPY migrations ./migrations
RUN touch src/main.rs && cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home --home-dir /app registry

COPY --from=builder /build/target/release/npm-registry-rust /usr/local/bin/npm-registry-rust

RUN mkdir -p /app/storage /app/data && chown -R registry:registry /app

WORKDIR /app
USER registry

ENV HOST=0.0.0.0 \
    PORT=4873 \
    STORAGE_DIR=/app/storage \
    DATABASE_URL=sqlite:///app/data/registry.db?mode=rwc \
    RUST_LOG=info

EXPOSE 4873

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:4873/api/health || exit 1

ENTRYPOINT ["/usr/local/bin/npm-registry-rust"]
