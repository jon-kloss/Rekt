# REKT — single static binary, embedded UI + migrations. Multi-stage so the
# runtime image carries only the binary + CA roots (for HTTPS to Finnhub /
# Alpaca / Anthropic). Pinned to the same toolchain as rust-toolchain.toml.

FROM rust:1.96-slim-bookworm AS builder
WORKDIR /app
# build-essential: the bundled SQLite (libsqlite3-sys) needs a C compiler.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p rekt-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/rekt /usr/local/bin/rekt
# Durable state lives here; mount a volume at /data to persist across deploys
# (omit the volume and every deploy is a clean slate — fine for the demo).
ENV REKT_DATA_DIR=/data
RUN mkdir -p /data
# No REKT_LISTEN: the binary binds 0.0.0.0:$PORT when a platform injects PORT
# (Railway/Heroku/etc.). Set REKT_DEMO=1 + the API keys as deploy env vars.
EXPOSE 8080
CMD ["rekt"]
