# frick-ironclaw/homelab/Dockerfile
#
# Builds the IronClaw image for **Frick**, the cluster-ops agent in
# Leo's homelab. The build context is the repository root (this fork
# of nearai/ironclaw) — the upstream Rust source is at the repo root,
# plus any patches we've layered on top.
#
# Mirrors upstream's Dockerfile (kept aligned via upstream-sync.yml)
# and adds the runtime tooling Frick needs in-cluster.

# syntax=docker/dockerfile:1

# ─── Stage 1: build ──────────────────────────────────────────────────
# Floating 1.x tag (same convention as vetinari-openfang): upstream
# ironclaw bumps rust-version regularly (1.96 as of 2026-07), so a
# pinned minor tag keeps breaking the image build.
FROM rust:1-slim-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev cmake gcc g++ ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Layer cache: lockfiles first
COPY Cargo.toml Cargo.lock ./

# Source — directories per upstream Dockerfile + IronClaw layout.
# 2026-06-11: vendor/ removed upstream (weave-core no longer vendored);
# crates/, tests/, registry/, channels-src/, tools-src/ and providers.json
# added to match upstream's root Dockerfile COPY set.
COPY src/          src/
COPY crates/       crates/
COPY tests/        tests/
COPY migrations/   migrations/
COPY registry/     registry/
COPY channels-src/ channels-src/
COPY tools-src/    tools-src/
COPY wit/          wit/
COPY skills/       skills/
COPY profiles/     profiles/
COPY providers.json providers.json
COPY build.rs    ./

# Cargo validates manifest targets; create stub for examples we don't ship
RUN mkdir -p examples && echo 'fn main(){}' > examples/test_heartbeat.rs

ARG LTO=true
ARG CODEGEN_UNITS=1
ENV CARGO_PROFILE_RELEASE_LTO=${LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CODEGEN_UNITS}

RUN cargo build --release --bin ironclaw \
 && strip target/release/ironclaw

RUN cargo install tilth --root /usr/local

# ─── Stage 2: runtime ────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl wget tini \
      libssl3 \
      python3 python3-pip \
      jq ripgrep git \
      postgresql-client \
      gnupg lsb-release \
 && rm -rf /var/lib/apt/lists/*

# kubectl matched to current K3s cluster
ARG KUBECTL_VERSION=v1.34.0
RUN curl -fsSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" \
      -o /usr/local/bin/kubectl \
 && chmod +x /usr/local/bin/kubectl

# Bitwarden CLI (`bw`) — credential lookups against the cluster's
# Vaultwarden (https://warden.leopaska.xyz). Persistent agent
# credentials arrive via SealedSecrets → envFrom; `bw` is here for
# ad-hoc skill-driven lookups (keep BW_CLIENTID/BW_CLIENTSECRET/
# BW_PASSWORD short-lived, set only in the skill that needs them).
RUN apt-get update && apt-get install -y --no-install-recommends unzip && rm -rf /var/lib/apt/lists/*
ARG BW_VERSION=2026.4.1
RUN ARCH="$(dpkg --print-architecture)" \
 && case "$ARCH" in \
      amd64) BW_ARCH=linux ;; \
      arm64) BW_ARCH=linux-arm64 ;; \
      *) echo "unsupported arch: $ARCH" >&2; exit 1 ;; \
    esac \
 && curl -fsSL "https://github.com/bitwarden/clients/releases/download/cli-v${BW_VERSION}/bw-oss-${BW_ARCH}-${BW_VERSION}.zip" \
        -o /tmp/bw.zip \
 && unzip -q /tmp/bw.zip -d /usr/local/bin \
 && chmod +x /usr/local/bin/bw \
 && rm -f /tmp/bw.zip

# gh CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ironclaw /usr/local/bin/ironclaw
COPY --from=builder /usr/local/bin/tilth          /usr/local/bin/tilth
COPY --from=builder /app/migrations               /app/migrations
COPY --from=builder /app/skills                   /usr/local/bin/skills

RUN useradd -m -u 1000 -s /bin/bash ironclaw \
 && mkdir -p /data /etc/ironclaw/persona \
 && chown -R ironclaw:ironclaw /data /etc/ironclaw

USER ironclaw
WORKDIR /home/ironclaw

EXPOSE 3000 9100

# Reborn serves /api/health (and binds it before Postgres-backed runtime
# assembly finishes, so this goes healthy while migrations still run).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
  CMD curl -fsS http://localhost:3000/api/health || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
# `--bind` was a v1 legacy flag and no longer exists. `--bin ironclaw` now
# builds crates/ironclaw_reborn_cli, which upstream renamed to the `ironclaw`
# package after deleting the legacy monolith, so the binary path is unchanged.
CMD ["/usr/local/bin/ironclaw", "serve", "--host", "0.0.0.0", "--port", "3000"]
