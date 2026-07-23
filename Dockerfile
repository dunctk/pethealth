FROM rust:1.97-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY templates ./templates
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --create-home pethealth && mkdir -p /persistent && chown pethealth:pethealth /persistent
COPY --from=builder /build/target/release/pethealth /usr/local/bin/pethealth
USER pethealth
ENV APP_HOST=0.0.0.0 APP_PORT=3000 PRODUCTION=true RUST_LOG=pethealth=info,tower_http=info
EXPOSE 3000
VOLUME ["/persistent"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD curl -fsS http://127.0.0.1:3000/healthz || exit 1
CMD ["/usr/local/bin/pethealth"]

