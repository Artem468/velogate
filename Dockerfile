FROM node:22-bookworm-slim AS editor
WORKDIR /app/editor
COPY editor/package.json editor/package-lock.json ./
RUN npm ci
COPY editor/ ./
RUN npm run build

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY assets ./assets
COPY --from=editor /app/editor/dist ./editor/dist
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/velogate /usr/local/bin/velogate
COPY examples ./examples
COPY README.md LICENSE ./
EXPOSE 8080
ENTRYPOINT ["velogate"]
CMD ["start", "--config", "/app/examples/main.gate", "--health-path", "/healthz", "--metrics-path", "/metrics"]
