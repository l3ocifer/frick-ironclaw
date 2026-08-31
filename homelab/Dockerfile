# frick-ironclaw/homelab/Dockerfile
#
# Builds the IronClaw image for **Frick**, the cluster-ops agent in
# Leo's homelab. The build context is the repository root (this fork
# of nearai/ironclaw) — the upstream Rust source is at the repo root,
# plus any patches we've layered on top.
#
# Mirrors upstream's root Dockerfile (kept aligned via upstream-sync.yml)
# and adds the runtime tooling Frick needs in-cluster.
#
# 2026-07-26: rebuilt for the Reborn runtime. The previous version copied
# `src/`, `build.rs` and `tools-src/`, all of which upstream deleted when it
# dropped the v1 monolith, so every build after the Reborn merge failed at the
# COPY step and the `:homelab` tag silently kept serving the old v1 image.
# Three structural changes came with that, all taken from upstream's Dockerfile:
#   - the binary is now `--package ironclaw` (crates/ironclaw_reborn_cli) built
#     under `--profile dist`, so it lands at target/dist/, not target/release/;
#   - crates/ironclaw_webui builds a frontend during `cargo build`, so the Rust
#     builder needs node + pnpm on PATH;
#   - the runtime needs docker/reborn/*.toml and the entrypoint, which installs
#     a default config and validates the profile before exec'ing the binary.

# syntax=docker/dockerfile:1

# ─── Node toolchain (WebUI frontend) ─────────────────────────────────
FROM node:24.20.0-bookworm-slim AS node_toolchain

# ─── Chef base ───────────────────────────────────────────────────────
# Floating 1.x tag (same convention as vetinari-openfang): upstream
# ironclaw bumps rust-version regularly (1.96 as of 2026-07), so a
# pinned minor tag keeps breaking the image build. Non-slim, because the
# workspace needs a full build toolchain.
FROM rust:1-bookworm AS chef

COPY --from=node_toolchain /usr/local/bin/node /usr/local/bin/node
COPY --from=node_toolchain /usr/local/lib/node_modules/ /usr/local/lib/node_modules/

RUN ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && ln -sf ../lib/node_modules/corepack/dist/corepack.js /usr/local/bin/corepack \
 && corepack enable pnpm \
 && cargo install --locked cargo-chef@0.1.77

WORKDIR /app

# ─── Stage: recipe ───────────────────────────────────────────────────
# The root Cargo.toml is both the workspace root and a member package, so
# cargo refuses to read the manifest without its declared targets present.
# Upstream stubs them; the real binary comes from crates/ironclaw_reborn_cli.
FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY tools/ironclaw_stress/ tools/ironclaw_stress/
COPY skills/ skills/
COPY tests/ tests/
COPY wit/ wit/
COPY providers.json providers.json
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && printf '\n' > src/lib.rs

RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage: dependency build ─────────────────────────────────────────
FROM chef AS deps

ENV CARGO_PROFILE_DIST_PANIC=abort \
    CARGO_PROFILE_DIST_CODEGEN_UNITS=1

COPY --from=planner /app/recipe.json recipe.json
COPY crates/ironclaw_webui/frontend/ crates/ironclaw_webui/frontend/
WORKDIR /app/crates/ironclaw_webui/frontend
RUN pnpm install --frozen-lockfile
WORKDIR /app
RUN cargo chef cook \
    --profile dist \
    --package ironclaw \
    --recipe-path recipe.json

# ─── Stage: build ────────────────────────────────────────────────────
FROM deps AS builder

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY tools/ironclaw_stress/ tools/ironclaw_stress/
COPY migrations/ migrations/
COPY skills/ skills/
COPY tests/ tests/
COPY wit/ wit/
COPY providers.json providers.json
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && printf '\n' > src/lib.rs

WORKDIR /app/crates/ironclaw_webui/frontend
RUN pnpm install --frozen-lockfile
WORKDIR /app

RUN cargo build \
    --profile dist \
    --package ironclaw \
    --bin ironclaw \
 && strip target/dist/ironclaw

# ─── Stage: runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl wget tini \
      libssl3 \
      python3 python3-pip \
      jq ripgrep git unzip \
      postgresql-client sqlite3 \
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

COPY --from=builder /app/target/dist/ironclaw /usr/local/bin/ironclaw

# Shipped profile configs and the entrypoint that installs one. The
# Deployment seeds its own config.toml onto the state PVC before the
# container starts, so in-cluster these are the fallback — but the
# entrypoint also validates that the profile's required sections are
# present, which turns a malformed seed into a clear startup error
# instead of a partially-initialised runtime.
COPY docker/reborn/config.toml                            /opt/ironclaw/reborn/config.toml
COPY docker/reborn/config.hosted-single-tenant.toml        /opt/ironclaw/reborn/config.hosted-single-tenant.toml
COPY docker/reborn/config.hosted-single-tenant-volume.toml /opt/ironclaw/reborn/config.hosted-single-tenant-volume.toml
COPY docker/reborn/config.production.toml                  /opt/ironclaw/reborn/config.production.toml
COPY docker/reborn/entrypoint.sh /usr/local/bin/ironclaw-reborn-entrypoint

# Upstream defaults IRONCLAW_REBORN_SERVE_HOST to loopback, which is right
# for `docker run` and wrong for a Pod: kubelet's probes and the Service
# both reach the container over the pod IP.
ENV HOME=/home/ironclaw \
    IRONCLAW_REBORN_LOG=info \
    IRONCLAW_REBORN_SERVE_HOST=0.0.0.0 \
    IRONCLAW_REBORN_SERVE_PORT=3000

RUN useradd -m -d /home/ironclaw -u 1000 -s /bin/bash ironclaw \
 && mkdir -p /data /workspace /etc/ironclaw/persona \
 && chown -R ironclaw:ironclaw /home/ironclaw /data /workspace /etc/ironclaw \
 && chmod +x /usr/local/bin/ironclaw-reborn-entrypoint

USER ironclaw
WORKDIR /workspace

EXPOSE 3000 9100

# Reborn serves /api/health (and binds it before Postgres-backed runtime
# assembly finishes, so this goes healthy while migrations still run).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
  CMD curl -fsS http://localhost:3000/api/health || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "ironclaw-reborn-entrypoint"]
# `--bind` was a v1 legacy flag and no longer exists. The entrypoint execs
# `ironclaw "$@"`, so these are the serve args; passing them explicitly keeps
# the bind address visible here rather than only in the image ENV above.
CMD ["serve", "--host", "0.0.0.0", "--port", "3000"]
