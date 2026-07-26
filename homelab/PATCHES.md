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
