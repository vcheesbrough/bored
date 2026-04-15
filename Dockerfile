# Iteration 1: backend only. Frontend stage added in iteration 2.
FROM rust:1.86@sha256:300ec56abce8cc9448ddea2172747d048ed902a3090e6b57babb2bf19f754081 AS base
WORKDIR /app
COPY . .

# check: run by CI via `docker build --target check .`
# This is the single source of truth for the Rust toolchain version.
FROM base AS check
RUN rustup component add rustfmt clippy
RUN cargo fmt -p backend -p shared --check
RUN cargo clippy -p backend -p shared -- -D warnings
RUN cargo test -p backend -p shared

FROM base AS builder
RUN cargo build --release -p backend

FROM debian:bookworm-slim@sha256:4724b8cc51e33e398f0e2e15e18d5ec2851ff0c2280647e1310bc1642182655d
RUN apt-get update \
    && apt-get install -y ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/backend ./backend
RUN openssl req -x509 -newkey rsa:4096 \
        -keyout /app/key.pem \
        -out /app/cert.pem \
        -days 3650 \
        -nodes \
        -subj "/CN=bored"
EXPOSE 443
CMD ["./backend"]
