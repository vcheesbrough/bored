# syntax=docker/dockerfile:1.7

FROM rust:1.94.1@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS frontend-builder
RUN rustup target add wasm32-unknown-unknown && cargo install trunk
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY frontend/ frontend/
COPY shared/ shared/
COPY backend/Cargo.toml backend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
RUN mkdir -p backend/src && touch backend/src/main.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs
# Release tag burned into the WASM bundle (see shared::app_version). Empty for
# local builds — the code then falls back to CARGO_PKG_VERSION. Kept below the
# toolchain layer so a tag change doesn't bust the cargo-install-trunk cache.
ARG RELEASE_TAG=""
ENV RELEASE_TAG=${RELEASE_TAG}
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    --mount=type=cache,id=bored-trunk-cache,target=/root/.cache/trunk,sharing=locked \
    cd frontend && trunk build --release

FROM rust:1.94.1@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS backend-builder
RUN rustup component add rustfmt clippy
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/
COPY shared/ shared/
COPY frontend/Cargo.toml frontend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
RUN mkdir -p frontend/src && touch frontend/src/lib.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    cargo fmt -p backend -p shared --check
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    cargo clippy -p backend -p shared -- -D warnings
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    cargo test -p backend -p shared --lib
# Release tag burned into the backend binary (see shared::app_version). Empty for
# local builds — the code then falls back to CARGO_PKG_VERSION. Kept below the
# fmt/clippy/test layers so a tag change only recompiles the crates that read it.
ARG RELEASE_TAG=""
ENV RELEASE_TAG=${RELEASE_TAG}
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    cargo build --release -p backend \
    && cp /app/target/release/backend /tmp/backend

FROM debian:trixie-slim@sha256:4ffb3a1511099754cddc70eb1b12e50ffdb67619aa0ab6c13fcd800a78ef7c7a
# Static OCI image metadata. Dynamic labels (version, revision, created) are set
# at build time via `docker build --label` in .woodpecker/build.yml.
LABEL org.opencontainers.image.title="bored" \
      org.opencontainers.image.description="bored — full-stack Rust Kanban board (Axum API + Leptos WASM SPA)" \
      org.opencontainers.image.licenses="PolyForm-Noncommercial-1.0.0" \
      org.opencontainers.image.url="https://github.com/vcheesbrough/bored" \
      org.opencontainers.image.source="https://github.com/vcheesbrough/bored" \
      org.opencontainers.image.documentation="https://github.com/vcheesbrough/bored/blob/main/README.md" \
      org.opencontainers.image.authors="Vincent Cheesbrough" \
      org.opencontainers.image.vendor="Vincent Cheesbrough" \
      org.opencontainers.image.base.name="debian:trixie-slim" \
      org.opencontainers.image.base.digest="sha256:4ffb3a1511099754cddc70eb1b12e50ffdb67619aa0ab6c13fcd800a78ef7c7a"
RUN apt-get update \
    && apt-get install -y --no-install-recommends busybox-static ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend-builder /tmp/backend ./backend
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
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD ["busybox", "wget", "--quiet", "--spider", "--no-check-certificate", "https://localhost:443/health"]
CMD ["./backend"]
