# Local patches vs upstream nearai/ironclaw

Track non-additive changes. Additive changes under `homelab/` don't
need entries.

## Active patches

### heartbeat-tool-exec: execute tool calls in the periodic heartbeat loop

- **Files**: `src/agent/heartbeat.rs`, `src/agent/agent_loop.rs`, `src/agent/commands.rs`, `src/main.rs`
- **Reason**: upstream's heartbeat turn renders tool calls but never
  executes them, so autonomous cron/heartbeat turns could not act.
  Adds tool execution to the heartbeat loop (originally shipped
  2026-06-25, `fix(heartbeat): execute tool calls in the periodic
  heartbeat loop`).
- **Upstream PR**: not yet submitted.
- **Last applied**: 2026-07-17 against upstream@81dbdc6d0 (clean merge,
  no reapply needed).

### safety-guards: command_guard + integrity modules (additive)

- **Files**: `src/safety/command_guard.rs`, `src/safety/integrity.rs`, `src/safety/mod.rs` (2 module hooks)
- **Reason**: homelab-side command-sandbox and integrity checks for
  autonomous cluster-ops turns. New files are additive; only the
  `mod.rs` hook lines can conflict with upstream.
- **Upstream PR**: not submitted.
- **Last applied**: 2026-07-17 against upstream@81dbdc6d0 (clean merge).
