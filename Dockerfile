# syntax=docker/dockerfile:1.7

FROM rust:1.98.1@sha256:462a9af3c54fb4718850d3c602fc0e54452c20b1c12a4e4080fdb001d4b9acbf AS frontend-builder
# `rust-toolchain.toml` at the repo root pins the same version for local dev;
# keep the two in sync when bumping.
# `--version` + `--locked` pin trunk and its whole dependency tree: an unlocked
# `cargo install trunk --version 0.21.14` fails to compile regardless of rustc
# version, because it resolves lightningcss 1.0.0-alpha.65's own dependency
# `parcel_selectors 0.28.3` against cssparser 0.37.0 while lightningcss and
# `cssparser-color` want cssparser 0.33.0 — a version conflict inside
# lightningcss's own Cargo.toml, not a rust-toolchain incompatibility.
# 0.21.14 remains the newest stable trunk release (0.22.0 has only shipped
# betas as of 2026-09).
RUN rustup target add wasm32-unknown-unknown && cargo install trunk --version 0.21.14 --locked
WORKDIR /app
# sovereign-config-provider is a private git dependency of `backend` only, but
# `trunk build` resolves the whole workspace Cargo.lock from within `frontend/`
# — on a cold cache this stage can race backend-builder's own fetch of the same
# dependency, so it needs the same credential rewrite (see backend-builder below
# and scripts/docker-git-credential.sh).
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
COPY Cargo.toml Cargo.lock ./
COPY frontend/ frontend/
COPY shared/ shared/
COPY backend/Cargo.toml backend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
COPY scripts/docker-git-credential.sh scripts/docker-git-credential.sh
RUN mkdir -p backend/src && touch backend/src/main.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs
# Release tag burned into the WASM bundle (see shared::app_version). Empty for
# local builds — the code then falls back to CARGO_PKG_VERSION. Kept below the
# toolchain layer so a tag change doesn't bust the cargo-install-trunk cache.
ARG RELEASE_TAG=""
ENV RELEASE_TAG=${RELEASE_TAG}
# This is the longest step in the build, and cargo says nothing between
# "Compiling frontend" and the finished artifact — so a slow compile and a hung
# one look identical, and neither says *why*.
#
# `-Ztime-passes` is rustc's own instrumentation: it prints each pass with its
# wall time and RSS as the pass completes. That distinguishes the cases that
# actually matter for this crate — `monomorphization_collector_graph_walk` and
# `type_check_crate` blowing up (Leptos `view!` nesting) versus `LLVM_passes`
# (codegen/optimisation) — which elapsed-time alone can never do.
#
# The flag is nightly-gated; `RUSTC_BOOTSTRAP=1` enables it on the pinned stable
# toolchain. It only affects diagnostics, never codegen. Setting it in RUSTFLAGS
# does change the fingerprint, so the first build after this lands recompiles;
# every build after that hits the cache as usual.
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    --mount=type=cache,id=bored-trunk-cache,target=/root/.cache/trunk,sharing=locked \
    --mount=type=secret,id=github_token \
    set -eu; \
    . /app/scripts/docker-git-credential.sh; \
    cd frontend && \
    RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Ztime-passes" CARGO_TERM_VERBOSE=true \
    trunk build --release

FROM rust:1.98.1@sha256:462a9af3c54fb4718850d3c602fc0e54452c20b1c12a4e4080fdb001d4b9acbf AS backend-builder
# `rust-toolchain.toml` at the repo root pins the same version for local dev;
# keep the two in sync when bumping.
RUN rustup component add rustfmt clippy
WORKDIR /app
# sovereign-config-provider is a private git dependency (see backend/Cargo.toml);
# cargo must shell out to `git` — rather than its built-in libgit2 fetcher — so
# it honours the GIT_CONFIG_* rewrite sourced from scripts/docker-git-credential.sh.
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/
COPY shared/ shared/
COPY scripts/docker-git-credential.sh scripts/docker-git-credential.sh
COPY frontend/Cargo.toml frontend/Cargo.toml
COPY mcp/Cargo.toml mcp/Cargo.toml
RUN mkdir -p frontend/src && touch frontend/src/lib.rs \
 && mkdir -p mcp/src && touch mcp/src/main.rs
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=secret,id=github_token \
    set -eu; \
    . /app/scripts/docker-git-credential.sh; \
    cargo fmt -p backend -p shared --check
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    --mount=type=secret,id=github_token \
    set -eu; \
    . /app/scripts/docker-git-credential.sh; \
    cargo clippy -p backend -p shared -- -D warnings
# No `--lib`: `backend` is a `[[bin]]`-only crate with no `[lib]` target, so
# `--lib` silently runs zero of its tests instead of erroring (confirmed by
# running the previous `--lib` invocation locally — it executed only
# `shared`'s 47 tests, never `backend`'s 91). Without it, cargo runs each
# package's actual target (backend's `src/main.rs` tests, shared's lib tests).
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    --mount=type=secret,id=github_token \
    set -eu; \
    . /app/scripts/docker-git-credential.sh; \
    cargo test -p backend -p shared
# Release tag burned into the backend binary (see shared::app_version). Empty for
# local builds — the code then falls back to CARGO_PKG_VERSION. Kept below the
# fmt/clippy/test layers so a tag change only recompiles the crates that read it.
ARG RELEASE_TAG=""
ENV RELEASE_TAG=${RELEASE_TAG}
RUN --mount=type=cache,id=bored-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=bored-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=bored-cargo-target,target=/app/target,sharing=locked \
    --mount=type=secret,id=github_token \
    set -eu; \
    . /app/scripts/docker-git-credential.sh; \
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
COPY --from=backend-builder /tmp/backend ./bored-backend
COPY --from=frontend-builder /app/frontend/dist ./dist
RUN openssl req -x509 -newkey rsa:4096 \
        -keyout /app/key.pem \
        -out /app/cert.pem \
        -days 3650 \
        -nodes \
        -subj "/CN=bored"
# BORED__SERVER__* — image-internal ServerConfig leaves (backend/src/config.rs).
# Never sourced from sovereign-config: identical across every deployment.
ENV BORED__SERVER__TLS_CERT=/app/cert.pem
ENV BORED__SERVER__TLS_KEY=/app/key.pem
ENV BORED__SERVER__STATIC_DIR=/app/dist
EXPOSE 443
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD ["busybox", "wget", "--quiet", "--spider", "--no-check-certificate", "https://localhost:443/health"]
CMD ["./bored-backend"]
