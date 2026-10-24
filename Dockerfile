# syntax=docker/dockerfile:1

# ---- build ------------------------------------------------------------------
# The bookworm (glibc) toolchain matches the runtime base; musl would break the
# native-cert lookup that the Binance WebSocket relies on.
FROM rust:1.98-bookworm AS build
WORKDIR /src

# Warm the dependency cache before the crate sources change. The stub sources
# compile the full dependency graph, so later edits only rebuild this crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src examples \
 && echo '' > src/lib.rs \
 && printf 'fn main() {}\n' > src/main.rs \
 && cargo build --locked --release --lib --bins \
 && rm -rf src examples

COPY src ./src
COPY examples ./examples
COPY tests ./tests
COPY static ./static
RUN cargo build --locked --release --bins --examples \
 && strip target/release/fx-sim-rust target/release/examples/admin_key

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates is required: the Binance WebSocket verifies TLS against the
# system trust store (rustls-native-certs), not the roots baked into reqwest.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl util-linux \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 fxsim \
 && useradd --system --uid 10001 --gid fxsim --home-dir /data --shell /usr/sbin/nologin fxsim

COPY --from=build /src/target/release/fx-sim-rust /usr/local/bin/fx-sim-rust
COPY --from=build /src/target/release/examples/admin_key /usr/local/bin/fx-admin-key
COPY --from=build /src/static /app/static
COPY docker/entrypoint.sh /usr/local/bin/fx-sim-entrypoint

RUN chmod 0755 /usr/local/bin/fx-sim-rust /usr/local/bin/fx-admin-key \
      /usr/local/bin/fx-sim-entrypoint \
 && mkdir -p /data \
 && chown fxsim:fxsim /data \
 && chmod 0700 /data

# Database, admin key and any sidecars live here; mount a volume to keep accounts.
VOLUME ["/data"]
EXPOSE 8000

# /api/symbols needs no auth and touches the database, so it proves both the
# HTTP layer and the SQLite handle are alive.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${FX_PORT:-8000}/api/symbols" > /dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/fx-sim-entrypoint"]
