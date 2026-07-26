# Retired: safety-guards

Homelab-side command-sandbox and integrity checks for autonomous
cluster-ops turns. These were additive modules at `src/command_guard.rs`
and `src/integrity.rs`, hooked in through `src/safety/mod.rs`.

Parked here on 2026-07-26 when upstream deleted the v1 legacy monolith
(root `src/`). The hook that included them went with it, so as of the
Reborn cutover nothing compiles or calls this code — the root package no
longer has a library or binary target at all. Leaving the files at
`src/safety/` would have implied the legacy tree partially survived.

They are kept rather than deleted because the intent still stands: guard
what an unattended cluster-ops turn is allowed to execute. To bring them
back, port them into `crates/ironclaw_safety` (upstream's safety crate)
and wire them through the Reborn tool-execution path, not through a
restored `src/safety/mod.rs`.

See `homelab/PATCHES.md` for the full retirement record.
