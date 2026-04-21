FROM rust:1.94.1@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS frontend-builder
RUN rustup target add wasm32-unknown-unknown && cargo install trunk
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY frontend/ frontend/
COPY shared/ shared/
COPY backend/Cargo.toml backend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
COPY agent/Cargo.toml agent/Cargo.toml
RUN mkdir -p backend/src && touch backend/src/main.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs \
 && mkdir -p agent/src && touch agent/src/main.rs
RUN cd frontend && trunk build --release

FROM rust:1.94.1@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS backend-builder
RUN rustup component add rustfmt clippy
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/
COPY shared/ shared/
COPY agent/ agent/
COPY frontend/Cargo.toml frontend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
RUN mkdir -p frontend/src && touch frontend/src/lib.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs
RUN cargo fmt -p backend -p shared -p agent --check
RUN cargo clippy -p backend -p shared -p agent -- -D warnings
RUN cargo test -p backend -p shared -p agent --lib
RUN cargo build --release -p backend -p agent

FROM debian:trixie-slim@sha256:4ffb3a1511099754cddc70eb1b12e50ffdb67619aa0ab6c13fcd800a78ef7c7a
RUN apt-get update \
    && apt-get install -y ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend-builder /app/target/release/backend ./backend
# agent-poc is built above for CI validation but not copied here — it requires
# the claude CLI on PATH and is deployed/run separately outside this image.
COPY --from=frontend-builder /app/frontend/dist ./dist
RUN openssl req -x509 -newkey rsa:4096 \
        -keyout /app/key.pem \
        -out /app/cert.pem \
        -days 3650 \
        -nodes \
        -subj "/CN=bored"
ENV TLS_CERT=/app/cert.pem
ENV TLS_KEY=/app/key.pem
ENV STATIC_DIR=/app/dist
EXPOSE 443
CMD ["./backend"]
