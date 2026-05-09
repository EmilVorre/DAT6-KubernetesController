# syntax=docker/dockerfile:1.7
#
# DAT6 controller — multi-stage build with BuildKit cache mounts.
#
# Cache mounts (registry / git / target) keep iteration fast: a no-op rebuild
# is ~10s, a single-file change is well under a minute. Without them every
# build re-downloads the kube-rs / k8s-openapi dependency tree (~5+ minutes).
#
# Build:
#   DOCKER_BUILDKIT=1 docker build -t ghcr.io/emilvorre/dat6-controller:latest .
# (Or via the Makefile: `make build-controller`.)
#
# Final image is debian-slim because reqwest + rustls still need a shared libc
# and ca-certificates for HTTPS to the apiserver. Distroless static would
# require switching the build to musl, which we deliberately avoid for now.

# Match the dev-host toolchain so `--locked` reproduces the same build the
# user ran locally. Floor is 1.85 (a transitive kube-rs dep — `home` >=
# 0.5.12 — uses edition 2024, only stabilized in 1.85), but pin to the
# exact dev version to avoid silent drift if Cargo.lock is regenerated
# locally with a newer feature gate. Bump alongside the host toolchain.
# (app/Dockerfile pins 1.83 and works because the app crate has a much
# smaller dep tree that doesn't pull in `home`.)
FROM rust:1.95-bookworm AS builder
WORKDIR /build

# Bring in just the controller crate. The repo also contains app/ (a separate
# crate); .dockerignore excludes it so the build context stays small and so a
# change to the app doesn't bust this image's cache.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# `--locked` ensures the build matches Cargo.lock exactly (no silent dep
# upgrades between local cargo run and the in-cluster image).
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked --bin DAT6_KubernetesController && \
    cp target/release/DAT6_KubernetesController /tmp/dat6-controller

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/dat6-controller /usr/local/bin/dat6-controller

# Run as a non-root UID. The controller only needs network egress to the
# apiserver and to pod IPs on :8080; no filesystem writes, no privileged caps.
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin controller
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/dat6-controller"]
