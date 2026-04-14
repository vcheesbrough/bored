# Iteration 1: backend only. Frontend stage added in iteration 2.
FROM rust:1.78 AS builder
WORKDIR /app
COPY . .
RUN cargo test -p backend -p shared --lib
RUN cargo build --release -p backend

FROM debian:bookworm-slim
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
