# REKT — single static binary, embedded UI + migrations. Multi-stage so the
# runtime image carries only the binary + CA roots (for HTTPS to Finnhub /
# Alpaca / Anthropic). Pinned to the same toolchain as rust-toolchain.toml.

# Exact patch matches rust-toolchain.toml so rustup doesn't re-download a pin.
FROM rust:1.96.0-slim-bookworm AS builder
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
# Run unprivileged; /data is owned by that user.
RUN useradd -r -u 10001 rekt && mkdir -p /data && chown rekt /data
USER rekt
# ⚠️  /data is EPHEMERAL unless you mount a volume here. The public DEMO wants
# that (every deploy is a clean, self-healing seed). A REAL deploy MUST mount a
# persistent volume at /data or the portfolio DB is LOST on every redeploy.
ENV REKT_DATA_DIR=/data
# Default bind so a plain `docker run -p 8080:8080` is reachable; platforms that
# inject $PORT (Railway/Heroku) override this. Set REKT_DEMO=1 + the API keys as
# deploy env vars. (REKT_LISTEN, if set, still wins over PORT.)
ENV PORT=8080
EXPOSE 8080
CMD ["rekt"]
