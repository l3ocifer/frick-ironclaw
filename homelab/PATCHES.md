# Local patches vs upstream nearai/ironclaw

Track non-additive changes. Additive changes under `homelab/` don't
need entries.

## Active patches

None. Both former patches were retired by the 2026-07-26 Reborn adoption
(below) because the code they patched no longer exists upstream.

## Retired patches

### heartbeat-tool-exec (retired 2026-07-26)

- **Files**: `src/agent/heartbeat.rs`, `src/agent/agent_loop.rs`, `src/agent/commands.rs`, `src/main.rs`
- **Was**: upstream's v1 heartbeat turn rendered tool calls but never
  executed them, so autonomous cron/heartbeat turns could not act.
- **Retired because**: upstream deleted the entire v1 legacy monolith
  these files belonged to. Reborn models heartbeat as a first-class
  `HeartbeatRequest` routed through `turn_run_executor`, the same executor
  that runs normal turns, so the gap the patch closed does not exist in
  the new architecture.

### safety-guards (retired 2026-07-26)

- **Files**: `src/safety/command_guard.rs`, `src/safety/integrity.rs`, `src/safety/mod.rs`
- **Was**: homelab-side command-sandbox and integrity checks for
  autonomous cluster-ops turns.
- **Retired because**: only ever wired in through the legacy
  `src/safety/mod.rs`, so it became dead code when that tree was deleted.
  Upstream now ships `crates/ironclaw_safety`. If these checks are still
  wanted, re-add them there rather than restoring the old module.

### `crates/ironclaw_skills/src/registry.rs` — sha256 hex format (retired 2026-07-17)

Upstream used `format!("sha256:{:x}", result)` which stopped compiling
with rustc 1.92 + edition 2024 + generic-array 0.14 (E0277). Our
`homelab/Dockerfile` carried a build-time `sed` swapping it for a
byte-iter hex encode. Upstream now uses `hex::encode` in registry.rs,
so the patch (and its Dockerfile `RUN`) was dropped with the
2026-07-17 upstream merge (`merge: upstream ironclaw @ 81dbdc6d0`).

## 2026-07-26: v1 → Reborn cutover

Upstream deleted the v1 legacy monolith (root `src/`, package
`ironclaw_legacy`, binary `ironclaw-legacy`) and renamed
`crates/ironclaw_reborn_cli` to package `ironclaw`, producing a binary
still called `ironclaw`. Same binary name, different runtime. What
changed on our side:

| | before | after |
|---|---|---|
| args | `--no-onboard run` | `serve --host 0.0.0.0 --port 3000` |
| config | `/etc/ironclaw/openclaw.json` | `$IRONCLAW_REBORN_HOME/config.toml` (`homelab/config/reborn-config.toml`, seeded onto the state PVC by an initContainer) |
| state | `/data` (SQLite sessions) | CNPG Postgres `ironclaw_frick`, via `IRONCLAW_REBORN_POSTGRES_URL` |
| health | `/api/health` | `/api/health` (unchanged) |
| LLM | `LLM_BACKEND` + `LLM_BASE_URL`/`LLM_MODEL`/`LLM_API_KEY` | `[llm.default]` with `provider_id = "openai_compatible"`, which reads the same `LLM_BASE_URL`/`LLM_API_KEY` |

No new secrets were needed: `DATABASE_URL`, `SECRETS_MASTER_KEY` and
`A2A_BEARER_TOKEN` already existed in the `frick-secrets` SealedSecret and
are mapped to the `IRONCLAW_REBORN_*` names the runtime expects.

### Follow-up: the cutover did not build (fixed 2026-07-26)

The first cutover changed the args, config and manifests but left
`homelab/Dockerfile` as the v1 build, which still copied `src/`, `build.rs`
and `tools-src/` (plus `channels-src/` on Vimes) — paths upstream deleted with
the monolith. Every build after the merge therefore failed at the COPY step,
and because a failed build pushes nothing, the `:homelab` tag kept resolving to
the *old v1 image*. ArgoCD reported Synced and Healthy throughout, on a pod that
had been running for 13 hours. Two lessons, both cheap to apply next time:

- A green ArgoCD and a Running pod say nothing about whether the image you
  intended to ship exists. Check the tag's digest against the deployed digest.
- The build had never been exercised, because `homelab-build-check.yml` is a
  *pull_request* gate and the cutover went straight to main. Push the build to
  a PR when the Dockerfile changes, or accept that main is the first test.

The Dockerfile is now a mirror of upstream's root Dockerfile plus our runtime
tooling. Three things about the Reborn build differ from the v1 one and are
easy to get wrong on the next sync:

- the target is `--package ironclaw --profile dist`, so the binary is at
  `target/dist/ironclaw`, not `target/release/`;
- `crates/ironclaw_webui` builds a frontend during `cargo build`, so the Rust
  builder needs node + pnpm (upstream copies them out of a `node:22` stage);
- the runtime stage needs `docker/reborn/*.toml` and `entrypoint.sh`.

`cargo install tilth` was dropped at the same time: nothing in the deployment,
skills or config referenced it, so it was only lengthening the build.

Two config-side bugs came out of the same review, neither of which the image
fix would have covered on its own:

- `homelab/config/reborn-config.toml` had no `[storage]` section. The
  `hosted-single-tenant` profile requires it — it is what points the runtime at
  `IRONCLAW_REBORN_POSTGRES_URL` and `IRONCLAW_REBORN_SECRET_MASTER_KEY` — and
  the entrypoint exits with a clear error when it is missing. Added, along with
  `runner.worker_count = 0` to match upstream's guidance for this profile.
- the Deployment set `command: ["ironclaw"]`, which replaces the image
  ENTRYPOINT and so skipped upstream's entrypoint entirely — including that
  `[storage]` check. Removed; `args:` alone now flows through to
  `exec ironclaw "$@"`, and tini is back as PID 1.

### What the first real Reborn boot demanded

Neither of these is discoverable from the config schema — the runtime only
rejects them while assembling, so they cost a crash-loop each:

- `IRONCLAW_REBORN_POSTGRES_RESOURCE_GOVERNOR_SINGLETON=true`. A required
  operator assertion that this process is the only resource-governor for its
  database. Setting it to `false` is also an error; the runtime wants the
  affirmative statement. True here because frick owns `ironclaw_frick` alone
  and `replicas: 1` + `strategy: Recreate` means no second pod overlaps —
  **scaling this Deployment past 1, or switching to RollingUpdate, silently
  invalidates it.**
- `DATABASE_SSLMODE=disable` plus
  `IRONCLAW_REBORN_ALLOW_REMOTE_POSTGRES_CLEAR_TEXT=true`. Reborn refuses a
  remote Postgres that is neither TLS-verified nor explicitly waived. TLS was
  attempted first: Reborn's `require` *verifies* the chain against rustls
  native roots and exposes no CA-file setting, CNPG signs with a per-cluster CA
  that lives in `homelab-pg-ca` in the `databases` namespace, and a Pod in
  `agents-shared` cannot mount it. Pinning the CA in git is worse than useless
  — CNPG rotates it every 90 days. The hostname itself was never the problem
  (`homelab-pg-rw.databases.svc.cluster.local` is in the cert SANs), so the day
  that CA is replicated into `agents-shared` automatically, flipping this back
  to `require` is the whole change.

### Not carried over — verify before trusting these

`openclaw.json` also configured things with no Reborn equivalent wired up
yet. Upstream's own cutover note is explicit that it "does not perform a v1
data migration". Treat these as unverified after the cutover:

- **fleet bus** — `fleet.databaseUrlEnv` / `notifyChannel: fleet_events_new`
  and the inbound severity predicate.
- **MCP servers** declared in `openclaw.json`.
- **cron / scheduled turns** — Reborn expresses these as triggers.
- **A2A peers** for cross-agent handoff.
- **existing session history** in the `/data` SQLite store; Reborn starts
  from its Postgres schema.

`openclaw.json` is still generated and mounted, unused by the runtime, so
it stays as the reference for re-wiring these and so a rollback is a pure
image + args revert.
