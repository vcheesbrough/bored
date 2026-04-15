# Iteration 1: backend only. Frontend stage added in iteration 2.
FROM rust:1.86@sha256:300ec56abce8cc9448ddea2172747d048ed902a3090e6b57babb2bf19f754081 AS builder
WORKDIR /app
COPY . .
RUN cargo test -p backend -p shared --lib
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
ENV TLS_CERT=/app/cert.pem
ENV TLS_KEY=/app/key.pem
EXPOSE 443
CMD ["./backend"]
