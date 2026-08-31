# Build only the relay. The `orange` crate needs GStreamer and never runs in a
# container, so `-p orange-relay` keeps this image small and dependency-free.

FROM rust:1.98-slim-bookworm AS build
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --locked --release -p orange-relay

FROM debian:bookworm-slim
# WebSockets over TLS terminate at the platform ingress, but outbound checks
# and general hygiene still want CA certificates present.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/orange-relay /usr/local/bin/orange-relay

# Container Apps injects PORT; the default matches local runs.
ENV PORT=9000
EXPOSE 9000

# Unprivileged: the relay needs nothing from the host.
RUN useradd --system --uid 10001 orange
USER 10001

CMD ["orange-relay"]
