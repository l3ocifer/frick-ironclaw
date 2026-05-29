# IronClaw Development Guide

**IronClaw** is a secure personal AI assistant — user-first security, self-expanding tools, defense in depth, multi-channel access with proactive background execution.

## Build & Test

```bash
cargo fmt                                                    # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                   # unit tests
cargo test --features integration                            # + PostgreSQL tests
RUST_LOG=ironclaw=debug cargo run                            # run with logging
```

E2E tests: see `tests/e2e/CLAUDE.md`.

## Code Style

- Prefer `crate::` for cross-module imports; `super::` is fine in tests and intra-module refs
- No `pub use` re-exports unless exposing to downstream consumers
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- Use `thiserror` for error types in `error.rs`
- Map errors with context: `.map_err(|e| SomeError::Variant { reason: e.to_string() })?`
- Prefer strong types over strings (enums, newtypes)
- Keep functions focused, extract helpers when logic is reused
- Comments for non-obvious logic only
- **Prompt templates live in files, not Rust code**: Multi-line prompt strings (mission goals, system prompts, CodeAct preambles) go in `crates/ironclaw_engine/prompts/*.md` and are loaded via `include_str!()`. Never inline large prompt templates as Rust string constants — they're hard to read, review, and iterate on. Single-line format strings are fine inline.
- **Logging levels matter for REPL/TUI**: `info!` and `warn!` output appears in the REPL and corrupts the terminal UI. Use `debug!` for internal diagnostics (trace analysis, reflection results, engine internals). Reserve `info!` for user-facing status that the REPL intentionally renders. Background tasks (reflection, trace analysis) must NEVER use `info!` — it breaks the interactive display.
- **Test through the caller, not just the helper**: When a predicate/classifier/transform helper gates a side effect (HTTP, DB write, OAuth, UI mutation, tool execution) and has any wrapper or computed input between it and that side effect, a unit test on the helper alone is *not* sufficient regression coverage. Add a test that drives the call site — typically a `*_handler`, `factory::create_*`, or `manager::*` — at the integration tier (`cargo test --features integration`) or higher. The same applies to test mocks: if you mock a multi-arg runtime API like `window.open(url, target, features)`, the mock must capture every argument the production caller passes. See `.claude/rules/testing.md` ("Test Through the Caller, Not Just the Helper") for the full rule and the bug examples that motivated it.

## Architecture

Prefer generic/extensible architectures over hardcoding specific integrations. Ask clarifying questions about the desired abstraction level before implementing.

### Extension/Auth Invariants

Extension and channel onboarding has two distinct identities that must not be conflated:

- `credential_name`: backend secret identity used for storage, injection, and gate resume
- `extension_name`: user-facing installed extension/channel identity used for setup routing and UI

Examples:

- Telegram:
  - `credential_name = telegram_bot_token`
  - `extension_name = telegram`
- Gmail:
  - `credential_name = google_oauth_token`
  - `extension_name = gmail`

Rules:

- Never route web setup/configure UI directly from `credential_name`.
- Chat and Settings must use the same setup/configure path for installable extensions/channels.
- Generic auth-card UI is only for non-extension credential prompts or pure OAuth launch prompts.
- If an auth flow is for an installed extension/channel, resolve the `extension_name` once in shared backend logic and carry it through the wire contract rather than re-deriving it in multiple layers.
- New auth/onboarding code must reuse the shared resolver/controller path instead of adding channel-specific or frontend-only fallbacks.

Current ownership:

- `src/bridge/auth_manager.rs`: canonical auth-flow extension-name resolver
- `src/bridge/router.rs`: auth gate display + submit routing
- `src/channels/web/server.rs`: pending-gate/history rehydration
- `crates/ironclaw_gateway/static/js/core/onboarding.js`: unified onboarding controller and configure-modal routing (previously in the monolithic `app.js`, now split — see `crates/ironclaw_gateway/src/assets.rs` for the concat order)

Temporary compatibility boundary:

- Web auth prompts with a gate `request_id` are the v2 path and must resolve through `/api/chat/gate/resolve`.
- Web auth prompts without a `request_id` are legacy engine v1 `pending_auth` compatibility only.
- Keep that compatibility isolated; do not add new features to it.
- Once v1 auth mode is removed, delete the legacy `/api/chat/auth-token` and `/api/chat/auth-cancel` shim endpoints and the matching no-`request_id` UI branch.

Key traits for extensibility: `Database`, `Channel`, `Tool`, `LlmProvider`, `SuccessEvaluator`, `EmbeddingProvider`, `NetworkPolicyDecider`, `Hook`, `Observer`, `Tunnel`.

All I/O is async with tokio. Use `Arc<T>` for shared state, `RwLock` for concurrent access.

**LLM data is never deleted.** All LLM output — context fed to the model, reasoning, tool calls, messages, events, steps — is the most valuable data in the system. Never strip, truncate, or delete it from the database. Mark with timestamps, make filterable, but always retain. In-memory HashMaps are caches; the database (via Workspace) is the source of truth. "Cleanup" means evicting from in-memory caches, never deleting database rows.

## Extracted Crates

Safety logic lives in `crates/ironclaw_safety/`, skills in `crates/ironclaw_skills/`, multi-provider LLM integration in `crates/ironclaw_llm/`. **Import directly from the extracted crate** (e.g. `use ironclaw_safety::SafetyLayer`, `use ironclaw_skills::SkillRegistry`, `use ironclaw_llm::{LlmProvider, LlmError}`). Do not use `crate::safety::`, `crate::skills::`, or `crate::llm::` for types that originate in extracted crates — `src/llm/` was deleted in the LLM extraction, and `src/safety/mod.rs` / `src/skills/mod.rs` no longer glob-re-export. Local items defined in those modules (e.g. `crate::skills::attenuate_tools`) are fine. The `crate::error::LlmError` alias and `crate::config::*Config` re-exports are kept as a thin convenience: they forward to `ironclaw_llm::*` so existing call sites compile, but new code should import from the extracted crate.

## Project Structure

```
crates/
├── ironclaw_safety/    # Extracted: prompt injection, validation, leak detection, policy
└── ironclaw_llm/       # Extracted: multi-provider LLM integration (rig-core, OpenAI, Anthropic, NEAR AI, Bedrock, …)

src/
├── lib.rs              # Library root, module declarations
├── main.rs             # Entry point, CLI args, startup
├── app.rs              # App startup orchestration (channel wiring, DB init)
├── bootstrap.rs        # Base directory resolution (~/.ironclaw), early .env loading
├── settings.rs         # User settings persistence (~/.ironclaw/settings.json)
├── service.rs          # OS service management (launchd/systemd daemon install)
├── tracing_fmt.rs      # Custom tracing formatter
├── util.rs             # Shared utilities
├── config/             # Configuration from env vars (split by subsystem)
│   ├── mod.rs          # Re-exports all config types; top-level Config struct
│   ├── agent.rs, llm.rs, channels.rs, database.rs, sandbox.rs, skills.rs
│   ├── heartbeat.rs, routines.rs, safety.rs, embeddings.rs, wasm.rs
│   ├── tunnel.rs       # Tunnel provider config (TUNNEL_PROVIDER, TUNNEL_URL, etc.)
│   └── secrets.rs, hygiene.rs, builder.rs, helpers.rs
├── error.rs            # Error types (thiserror)
│
├── agent/              # Core agent loop, dispatcher, scheduler, sessions — see src/agent/CLAUDE.md
│
├── channels/           # Multi-channel input
│   ├── channel.rs      # Channel trait, IncomingMessage, OutgoingResponse
│   ├── manager.rs      # ChannelManager merges streams
│   ├── cli/            # Full TUI with Ratatui
│   ├── http.rs         # HTTP webhook (axum) with secret validation
│   ├── webhook_server.rs # Unified HTTP server composing all webhook routes
│   ├── repl.rs         # Simple REPL (for testing)
│   ├── web/            # Web gateway (browser UI) — see src/channels/web/CLAUDE.md
│   └── wasm/           # WASM channel runtime
│       ├── mod.rs
│       ├── bundled.rs  # Bundled channel discovery
│       ├── capabilities.rs # Channel-specific capabilities (HTTP endpoint, emit rate)
│       ├── error.rs    # WASM channel error types
│       ├── runtime.rs  # WASM channel execution runtime
│       ├── setup.rs    # WasmChannelSetup, setup_wasm_channels(), inject_channel_credentials()
│       └── wrapper.rs  # Channel trait wrapper for WASM modules
│
├── cli/                # CLI subcommands (clap)
│   ├── mod.rs          # Cli struct, Command enum (run/onboard/config/tool/registry/mcp/memory/pairing/service/doctor/status/completion)
│   └── config.rs, tool.rs, registry.rs, mcp.rs, memory.rs, pairing.rs, service.rs, doctor.rs, status.rs, completion.rs
│
├── registry/           # Extension registry catalog
│   ├── manifest.rs     # ExtensionManifest, ArtifactSpec, BundleDefinition types
│   ├── catalog.rs      # RegistryCatalog: load from filesystem and embedded JSON
│   └── installer.rs    # RegistryInstaller: download, verify, install WASM artifacts
│
├── hooks/              # Lifecycle hooks (6 points: BeforeInbound, BeforeToolCall, BeforeOutbound, OnSessionStart, OnSessionEnd, TransformResponse)
│
├── tunnel/             # Tunnel abstraction for public internet exposure
│   ├── mod.rs          # Tunnel trait, TunnelProviderConfig, create_tunnel(), start_managed_tunnel()
│   ├── cloudflare.rs   # CloudflareTunnel (cloudflared binary)
│   ├── ngrok.rs        # NgrokTunnel
│   ├── tailscale.rs    # TailscaleTunnel (serve/funnel modes)
│   ├── custom.rs       # CustomTunnel (arbitrary command with {host}/{port})
│   └── none.rs         # NoneTunnel (local-only, no exposure)
│
├── observability/      # Pluggable event/metric recording (noop, log, multi)
│
├── orchestrator/       # Internal HTTP API for sandbox containers
│   ├── api.rs          # Axum endpoints (LLM proxy, events, prompts)
│   ├── auth.rs         # Per-job bearer token store
│   └── job_manager.rs  # Container lifecycle (create, stop, cleanup)
│
├── worker/             # Runs inside Docker containers
│   ├── container.rs    # Container worker runtime (ContainerDelegate + shared agentic loop)
│   ├── job.rs          # Background job worker (JobDelegate + shared agentic loop)
│   ├── claude_bridge.rs # Claude Code bridge (spawns claude CLI)
│   └── proxy_llm.rs    # LlmProvider that proxies through orchestrator
│
├── safety/             # Re-export shim for crates/ironclaw_safety (see Extracted Crates)
│
├── llm/                # LLM integration (6 backends + intelligent router)
│   ├── provider.rs     # LlmProvider trait, message types
│   ├── mod.rs          # Provider factory (create_llm_provider)
│   ├── nearai.rs       # NEAR AI chat-api (Responses API)
│   ├── nearai_chat.rs  # NEAR AI Chat Completions API
│   ├── rig_adapter.rs  # rig-core adapter for OpenAI/Anthropic/Gemini/Ollama
│   ├── failover.rs     # Multi-provider failover with retryable error classification
│   ├── router.rs       # Intelligent LLM router (15-dim classifier, 4 profiles, 24 models)
│   ├── costs.rs        # Per-model cost lookup table
│   ├── reasoning.rs    # Planning, tool selection, evaluation
│   ├── retry.rs        # Retry logic with backoff
│   └── session.rs      # Session token management with auto-renewal
│
├── tools/              # Extensible tool system
│   ├── tool.rs         # Tool trait, ToolOutput, ToolError
│   ├── registry.rs     # ToolRegistry for discovery
│   ├── rate_limiter.rs # Shared sliding-window rate limiter
│   ├── builtin/        # Built-in tools (echo, time, json, http, web_fetch, file, shell, memory, message, job, routine, extension_tools, skill_tools, secrets_tools)
│   ├── builder/        # Dynamic tool building
│   │   ├── core.rs     # BuildRequirement, SoftwareType, Language
│   │   ├── templates.rs # Project scaffolding
│   │   ├── testing.rs  # Test harness integration
│   │   └── validation.rs # WASM validation
│   ├── mcp/            # Model Context Protocol
│   │   ├── client.rs   # MCP client over HTTP
│   │   ├── factory.rs  # create_client_from_config() — transport dispatch factory
│   │   ├── protocol.rs # JSON-RPC types
│   │   └── session.rs  # MCP session management (Mcp-Session-Id header, per-server state)
│   └── wasm/           # Full WASM sandbox (wasmtime)
│       ├── runtime.rs  # Module compilation and caching
│       ├── wrapper.rs  # Tool trait wrapper for WASM modules
│       ├── host.rs     # Host functions (logging, time, workspace)
│       ├── limits.rs   # Fuel metering and memory limiting
│       ├── allowlist.rs # Network endpoint allowlisting
│       ├── credential_injector.rs # Safe credential injection
│       ├── loader.rs   # WASM tool discovery from filesystem
│       ├── rate_limiter.rs # Per-tool rate limiting
│       ├── error.rs    # WASM-specific error types
│       └── storage.rs  # Linear memory persistence
│
├── db/                 # Dual-backend persistence (PostgreSQL + libSQL) — see src/db/CLAUDE.md
│
├── workspace/          # Persistent memory system — see src/workspace/README.md
│
├── context/            # Job context isolation (JobState, JobContext, ContextManager)
├── estimation/         # Cost/time/value estimation with EMA learning
├── evaluation/         # Success evaluation (rule-based, LLM-based)
│
├── sandbox/            # Docker execution sandbox
│   ├── config.rs       # SandboxConfig, SandboxPolicy enum (ReadOnly/WorkspaceWrite/FullAccess)
│   ├── manager.rs      # SandboxManager orchestration
│   ├── container.rs    # ContainerRunner, Docker lifecycle
│   └── proxy/          # Network proxy: domain allowlist, credential injection, CONNECT tunnel
│
├── secrets/            # Secrets management (AES-256-GCM, OS keychain for master key)
│
├── profile.rs          # Psychographic profile types, 9-dimension analysis framework
│
├── setup/              # 7-step onboarding wizard — see src/setup/README.md
│
├── skills/             # SKILL.md prompt extension system — see .claude/rules/skills.md
│
└── history/            # Persistence (PostgreSQL repositories, analytics)

tests/
├── *.rs                # Integration tests (workspace, heartbeat, WS gateway, pairing, etc.)
├── test-pages/         # HTML→Markdown conversion fixtures
└── e2e/                # Python/Playwright E2E scenarios (see tests/e2e/CLAUDE.md)
```

## Database

Dual-backend: PostgreSQL + libSQL/Turso. **All new persistence features must support both backends.** See `src/db/CLAUDE.md` and `.claude/rules/database.md`.

## Module Specs

When modifying a module with a spec, read the spec first. Code follows spec; spec is the tiebreaker.

**Module-owned initialization:** Module-specific initialization logic (database connection, transport creation, channel setup) must live in the owning module as a public factory function — not in `main.rs` or `app.rs`. These entry-point files orchestrate calls to module factories. Feature-flag branching (`#[cfg(feature = ...)]`) must be confined to the module that owns the abstraction.

| Module | Spec |
|--------|------|
| `src/agent/` | `src/agent/CLAUDE.md` |
| `src/channels/web/` | `src/channels/web/CLAUDE.md` |
| `src/db/` | `src/db/CLAUDE.md` |
| `crates/ironclaw_llm/` | `crates/ironclaw_llm/CLAUDE.md` |
| `src/setup/` | `src/setup/README.md` |
| `src/tools/` | `src/tools/README.md` |
| `src/workspace/` | `src/workspace/README.md` |
| `crates/ironclaw_engine/` | `crates/ironclaw_engine/CLAUDE.md` |
| `crates/ironclaw_reborn_webui_ingress/` | `crates/ironclaw_reborn_webui_ingress/CLAUDE.md` |
| `tests/e2e/` | `tests/e2e/CLAUDE.md` |

## Job State Machine

```
Pending -> InProgress -> Completed -> Submitted -> Accepted
    \                \-> Failed
     \-> Failed       \-> Stuck -> InProgress (recovery)
                              \-> Failed
```

## Skills System

SKILL.md files extend the agent's prompt with domain-specific instructions. See `.claude/rules/skills.md` for full details.

- **Trust model**: Trusted (user-placed in `~/.ironclaw/skills/` or workspace `skills/`, full tool access) vs Installed (registry, read-only tools)
- **Selection pipeline**: gating (check bin/env/config requirements) -> scoring (keywords/patterns/tags) -> budget (fit within `SKILLS_MAX_TOKENS`) -> attenuation (trust-based tool ceiling)
- **Skill tools**: `skill_list`, `skill_search`, `skill_install`, `skill_remove`

## Configuration

See `.env.example` for all environment variables. LLM backends (`nearai`, `openai`, `anthropic`, `ollama`, `openai_compatible`, `tinfoil`, `bedrock`) documented in `crates/ironclaw_llm/CLAUDE.md`.

## Adding a New Channel

1. Create `src/channels/my_channel.rs`
2. Implement the `Channel` trait
3. Add config in `src/config/channels.rs`
4. Wire up in `src/app.rs` channel setup section

## Everything Goes Through Tools

**Core principle**: all actions originating from gateway handlers, CLI
commands, routine engine, WASM channels, or any other non-agent caller
MUST go through `ToolDispatcher::dispatch()` — never directly through
`state.store`, `workspace`, `extension_manager`, `skill_registry`, or
`session_manager`.

This gives every UI-initiated mutation the same audit trail
(`ActionRecord`), safety pipeline (param validation, sensitive-param
redaction, output sanitization), and channel-agnostic surface as
agent-initiated tool calls. Channels are interchangeable extensions;
routing through one dispatch function means new channels inherit the
full pipeline for free.

The pre-commit hook (`scripts/pre-commit-safety.sh`) flags newly-added
lines in handler/CLI files that touch
`state.{store,workspace,extension_manager,skill_registry,session_manager}.*`
directly. Annotate intentional exceptions (rare — usually only read
aggregation across multiple users) with a trailing
`// dispatch-exempt: <reason>` comment on the same line. The check only
sees added lines, so existing untouched code doesn't trip during
incremental migration.

See `.claude/rules/tools.md` for the full pattern, allowed exemptions,
and migration status. The dispatcher itself lives in
`src/tools/dispatch.rs`.

## Engine v2 Per-Project Sandbox

When `SANDBOX_ENABLED=true`, engine v2 routes the five filesystem/shell tools
(`file_read`, `file_write`, `list_dir`, `apply_patch`, `shell`) for `/project/`
paths through a per-project Docker container instead of the host filesystem.
The host's directory at `~/.ironclaw/projects/<user_id>/<project_id>/` is bind-mounted at
`/project/` inside the container, and a `sandbox_daemon` binary inside the
container speaks NDJSON over `docker exec -i`.

When unset, the same code path uses a host-filesystem `MountBackend` —
behavior is unchanged. See `docs/plans/2026-04-10-engine-v2-sandbox.md`.

Build the sandbox image: `docker build -f crates/Dockerfile.sandbox -t ironclaw/sandbox:dev .`

## Workspace & Memory

Persistent memory with hybrid search (FTS + vector via RRF). Four tools: `memory_search`, `memory_write`, `memory_read`, `memory_tree`. Identity files (AGENTS.md, SOUL.md, USER.md, IDENTITY.md) injected into system prompt. Heartbeat system runs proactive periodic execution (default: 30 minutes), reading `HEARTBEAT.md` and notifying via channel if findings. See `src/workspace/README.md`.

## Debugging

```bash
RUST_LOG=ironclaw=trace cargo run           # verbose
RUST_LOG=ironclaw::agent=debug cargo run    # agent module only
RUST_LOG=ironclaw=debug,tower_http=debug cargo run  # + HTTP request logging
```

## Current Limitations

Some modules have a `README.md` that serves as the authoritative specification
for that module's behavior. When modifying code in a module that has a spec:

1. **Read the spec first** before making changes
2. **Code follows spec**: if the spec says X, the code must do X
3. **Update both sides**: if you change behavior, update the spec to match;
   if you're implementing a spec change, update the code to match
4. **Spec is the tiebreaker**: when code and spec disagree, the spec is correct
   (unless the spec is clearly outdated, in which case fix the spec first)

| Module | Spec File |
|--------|-----------|
| `src/setup/` | `src/setup/README.md` |

## Code Style

- Use `crate::` imports, not `super::`
- No `pub use` re-exports unless exposing to downstream consumers
- Prefer strong types over strings (enums, newtypes)
- Keep functions focused, extract helpers when logic is reused
- Comments for non-obvious logic only

## Review & Fix Discipline

Hard-won lessons from code review -- follow these when fixing bugs or addressing review feedback.

### Fix the pattern, not just the instance
When a reviewer flags a bug (e.g., TOCTOU race in INSERT + SELECT-back), search the entire codebase for all instances of that same pattern. A fix in `SecretsStore::create()` that doesn't also fix `WasmToolStore::store()` is half a fix.

### Propagate architectural fixes to satellite types
If a core type changes its concurrency model (e.g., `LibSqlBackend` switches to connection-per-operation), every type that was handed a resource from the old model (e.g., `LibSqlSecretsStore`, `LibSqlWasmToolStore` holding a single `Connection`) must also be updated. Grep for the old type across the codebase.

### Schema translation is more than DDL
When translating a database schema between backends (PostgreSQL to libSQL, etc.), check for:
- **Indexes** -- diff `CREATE INDEX` statements between the two schemas
- **Seed data** -- check for `INSERT INTO` in migrations (e.g., `leak_detection_patterns`)
- **Semantic differences** -- document where SQL functions behave differently (e.g., `json_patch` vs `jsonb_set`)

### Feature flag testing
When adding feature-gated code, test compilation with each feature in isolation:
```bash
cargo check                                          # default features
cargo check --no-default-features --features libsql  # libsql only
cargo check --all-features                           # all features
```
Dead code behind the wrong `#[cfg]` gate will only show up when building with a single feature.

### Mechanical verification before committing
Run these checks on changed files before committing:
- `grep -rnE '\.unwrap\(|\.expect\(' <files>` -- no panics in production
- `grep -rn 'super::' <files>` -- use `crate::` imports
- If you fixed a pattern bug, `grep` for other instances of that pattern across `src/`

## Workspace & Memory System

Inspired by [OpenClaw](https://github.com/openclaw/openclaw), the workspace provides persistent memory for agents with a flexible filesystem-like structure.

### Key Principles

1. **"Memory is database, not RAM"** - If you want to remember something, write it explicitly
2. **Flexible structure** - Create any directory/file hierarchy you need
3. **Self-documenting** - Use README.md files to describe directory structure
4. **Hybrid search** - Combines FTS (keyword) + vector (semantic) via Reciprocal Rank Fusion

### Filesystem Structure

```
workspace/
├── README.md              <- Root runbook/index
├── MEMORY.md              <- Long-term curated memory
├── HEARTBEAT.md           <- Periodic checklist
├── IDENTITY.md            <- Agent name, nature, vibe
├── SOUL.md                <- Core values
├── AGENTS.md              <- Behavior instructions
├── USER.md                <- User context
├── context/               <- Identity-related docs
│   ├── vision.md
│   └── priorities.md
├── daily/                 <- Daily logs
│   ├── 2024-01-15.md
│   └── 2024-01-16.md
├── projects/              <- Arbitrary structure
│   └── alpha/
│       ├── README.md
│       └── notes.md
└── ...
```

### Using the Workspace

```rust
use crate::workspace::{Workspace, OpenAiEmbeddings, paths};

// Create workspace for a user
let workspace = Workspace::new("user_123", pool)
    .with_embeddings(Arc::new(OpenAiEmbeddings::new(api_key)));

// Read/write any path
let doc = workspace.read("projects/alpha/notes.md").await?;
workspace.write("context/priorities.md", "# Priorities\n\n1. Feature X").await?;
workspace.append("daily/2024-01-15.md", "Completed task X").await?;

// Convenience methods for well-known files
workspace.append_memory("User prefers dark mode").await?;
workspace.append_daily_log("Session note").await?;

// List directory contents
let entries = workspace.list("projects/").await?;

// Search (hybrid FTS + vector)
let results = workspace.search("dark mode preference", 5).await?;

// Get system prompt from identity files
let prompt = workspace.system_prompt().await?;
```

### Memory Tools

Four tools for LLM use:

- **`memory_search`** - Hybrid search, MUST be called before answering questions about prior work
- **`memory_write`** - Write to any path (memory, daily_log, or custom paths)
- **`memory_read`** - Read any file by path
- **`memory_tree`** - View workspace structure as a tree (depth parameter, default 1)

### Hybrid Search (RRF)

Combines full-text search and vector similarity using Reciprocal Rank Fusion:

```
score(d) = Σ 1/(k + rank(d)) for each method where d appears
```

Default k=60. Results from both methods are combined, with documents appearing in both getting boosted scores.

**Backend differences:**
- **PostgreSQL:** `ts_rank_cd` for FTS, pgvector cosine distance for vectors, full RRF
- **libSQL:** FTS5 for keyword search only (vector search via `libsql_vector_idx` not yet wired)

### Heartbeat System

Proactive periodic execution (default: 30 minutes):

1. Reads `HEARTBEAT.md` checklist
2. Runs agent turn with checklist prompt
3. If findings, notifies via channel
4. If nothing, agent replies "HEARTBEAT_OK" (no notification)

```rust
use crate::agent::{HeartbeatConfig, spawn_heartbeat};

let config = HeartbeatConfig::default()
    .with_interval(Duration::from_secs(60 * 30))
    .with_notify("user_123", "telegram");

spawn_heartbeat(config, workspace, llm, response_tx);
```

### Chunking Strategy

Documents are chunked for search indexing:
- Default: 800 words per chunk (roughly 800 tokens for English)
- 15% overlap between chunks for context preservation
- Minimum chunk size: 50 words (tiny trailing chunks merge with previous)

---

## Custom Extensions (Fork-Specific)

The following sections document features added in the `l3ocifer/ironclaw` fork on top of the upstream `nearai/ironclaw` codebase.

### OpenClaw Memory Transfer

Ported from [OpenClaw](https://github.com/openclaw/openclaw) (TypeScript) to Rust. Full plan: [`docs/MEMORY_TRANSFER_PLAN.md`](docs/MEMORY_TRANSFER_PLAN.md).

| Feature | File | Description |
|---------|------|-------------|
| Session save on `/new` | `src/agent/agent_loop.rs` | Last 15 messages saved to `daily/YYYY-MM-DD-session-HHMMSS.md` |
| Pre-compaction memory flush | `src/agent/agent_loop.rs` | Silent LLM turn before compaction to persist durable notes |
| MEMORY.md main-session only | `src/agent/agent_loop.rs` | MEMORY.md excluded from group chats for privacy |
| Logseq integration | `src/workspace/logseq.rs` | Reads user profile, preferences, decisions from Logseq graph |
| BOOT.md on startup | `src/agent/agent_loop.rs` | Runs BOOT.md as first agent turn with full tool access on startup |
| Memory flush with tools | `src/agent/agent_loop.rs` | Pre-compaction flush now executes memory tools (max 3 iterations) |
| Daily session reset | `src/agent/agent_loop.rs` | Auto-resets sessions at configurable hour boundary |
| Learnings system | `src/workspace/learnings.rs` | Evidence-backed rules with confidence scoring and lifecycle |
| Learning tools | `src/tools/builtin/learning.rs` | learning_create, learning_search, learning_promote |
| Salience scoring | `src/agent/compressor/salience.rs` | Turn/session importance scoring for intelligent compaction |
| Content-hash dedup | `src/workspace/mod.rs`, `repository.rs` | Cross-machine session merge deduplication |
| AGENTS.md template | `docs/reference/AGENTS.recommended.md` | Recommended workspace instructions template |

### Multi-Agent Task Graph

Inspired by [beads](https://github.com/steveyegge/beads). PostgreSQL DAG for task coordination across agents.

| File | Purpose |
|------|---------|
| `migrations/V9__agent_tasks.sql` | Schema: `agent_tasks`, `agent_task_deps`, `agent_task_events` tables |
| `src/workspace/tasks.rs` | `TaskRepository` with CRUD, cycle detection, auto-promotion, **memory decay** (`archive_completed_tasks`) |
| `src/tools/builtin/task.rs` | 6 LLM tools: `task_create`, `task_list`, `task_update`, `task_ready`, `task_export`, **`task_archive`** |

### Semantic Merge

Uses vendored [weave-core](https://github.com/Ataraxy-Labs/weave) for entity-level 3-way merge.

| File | Purpose |
|------|---------|
| `src/workspace/merge.rs` | `semantic_merge`, `merge_prefer_ours`, `merge_with_markers` |
| `src/workspace/mod.rs` | **`write_with_merge()`** — auto-merge on concurrent workspace writes |
| `vendor/weave-core/` | Vendored (upstream pins sem-core 0.2, we need 0.3) |

### Sandboxed Python

Uses [monty](https://github.com/pydantic/monty) (Rust-native Python interpreter) with resource limits.

| File | Purpose |
|------|---------|
| `src/tools/builtin/python.rs` | `PythonTool` — time/memory limited, no I/O, **external function bridge** (json_parse, json_dump, base64_encode, base64_decode, hash_sha256) |

### Security Hardening

| File | Purpose |
|------|---------|
| `src/safety/command_guard.rs` | Destructive command blocking — **20 security packs** (git, filesystem, database, containers, cloud, system, piped exec, inline scripts, sensitive paths, storage, secrets, remote, CI/CD, networking, DNS, backup, messaging, search, package managers, env vars) |
| `src/safety/integrity.rs` | Workspace identity file SHA-256 drift detection, **wired into heartbeat** |
| `src/tools/wasm/verification.rs` | WASM tool checksum verification on load |

### Token Compression

Ported from [claw-compactor](https://github.com/aeromomo/claw-compactor) — 5-stage deterministic compression pipeline.

| File | Purpose |
|------|---------|
| `src/agent/compressor/mod.rs` | `CompressorPipeline` with config, token estimation |
| `src/agent/compressor/observations.rs` | **Observation extraction** — highest-savings layer (~97%) |
| `src/agent/compressor/dedup.rs` | Shingle hashing + Jaccard similarity dedup |
| `src/agent/compressor/dictionary.rs` | Auto-learned codebook with `$XX` codes |
| `src/agent/compressor/patterns.rs` | Path shorthand, IP compression, enum compaction |
| `src/agent/compressor/text_optimizer.rs` | CJK normalization, whitespace, table compaction |
| `src/agent/compaction.rs` | **Wired**: compressor runs before LLM summarization |

### Agent Skills

97 bundled skills in `skills/` at repo root. Auto-discovered from exe dir, ancestor dirs, or CWD. Sourced from 24 reference repos.

| File | Purpose |
|------|---------|
| `src/skills/mod.rs` | Module root, public exports |
| `src/skills/loader.rs` | Multi-source discovery (`resolve_bundled_skills_dir`) |
| `src/skills/frontmatter.rs` | YAML frontmatter parser for SKILL.md |
| `src/skills/eligibility.rs` | OS, bins, env var checks |
| `src/skills/prompt.rs` | XML prompt formatting per Agent Skills standard |

### Additional Dependencies

| Crate | Source | Notes |
|-------|--------|-------|
| `weave-core` | `vendor/weave-core/` (path) | Vendored from Ataraxy-Labs/weave rev `8c461b4`. sem-core bumped 0.2→0.3. |
| `monty` | `git` rev `dcdf702` | Sandboxed Python interpreter. |

### Environment Variables (Fork-Specific)

| Variable | Default | Description |
|----------|---------|-------------|
| `AGENT_ID` | lowercase `AGENT_NAME` | Unique ID for multi-agent scoping (e.g., `frack`, `frick`) |
| `IRONCLAW_SKILLS_DIR` | (auto-detected) | Override bundled skills directory |

### LLM Provider Configuration

IronClaw supports 6 LLM backends. Set `LLM_BACKEND` env var or `llm_backend` in settings.

| Backend | `LLM_BACKEND` value | Required env vars | Default model |
|---------|---------------------|-------------------|---------------|
| NEAR AI (default) | `nearai` | `NEARAI_SESSION_TOKEN` or `NEARAI_API_KEY` | `llama4-maverick-instruct-basic` |
| Ollama (local) | `ollama` | None | `qwen3-coder:30b` |
| OpenAI | `openai` | `OPENAI_API_KEY` | `gpt-5.3-codex` |
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | `claude-sonnet-4-20250514` |
| Gemini | `gemini` | `GEMINI_API_KEY` | `gemini-2.5-pro` |
| OpenAI-compatible | `openai_compatible` | `LLM_BASE_URL` | `default` |

**Ollama-specific vars:**
- `OLLAMA_BASE_URL` — default `http://localhost:11434` (Frack: local, Frick: `http://alef:11434`)
- `OLLAMA_MODEL` — default `qwen3-coder:30b`

**Recommended setup for self-hosted:**
```bash
# Frack (MacBook) — use local Ollama
export LLM_BACKEND=ollama
export OLLAMA_MODEL=qwen3-coder:30b

# Frick (homelab) — use Ollama on alef
export LLM_BACKEND=ollama
export OLLAMA_BASE_URL=http://localhost:11434
export OLLAMA_MODEL=qwen3-coder:30b

# Cloud fallback (any agent)
export ANTHROPIC_API_KEY=sk-ant-...   # Claude Opus 4.6
export OPENAI_API_KEY=sk-...          # ChatGPT 5.3
export GEMINI_API_KEY=...             # Gemini 2.5 Pro
```

API keys from the encrypted secrets store are auto-injected via `inject_llm_keys_from_secrets()`.

### Intelligent LLM Router

IronClaw includes a native intelligent routing engine (`src/llm/router.rs`) ported from ClawRouter. It classifies requests by complexity and routes to the optimal model for cost/quality balance — all in <1ms with no external API calls.

**15-dimension weighted scoring:**
Token count, code presence, reasoning markers, technical terms, creative markers, simple indicators, multi-step patterns, question complexity, imperative verbs, constraints, output format, references, negation, domain specificity, agentic task detection.

**4 complexity tiers:** SIMPLE → MEDIUM → COMPLEX → REASONING

**4 routing profiles:**
| Profile | `ROUTING_PROFILE` | Behavior |
|---------|-------------------|----------|
| Auto (default) | `auto` | Standard routing with automatic agentic detection |
| Eco | `eco` | Ultra cost-optimized: cheapest viable models |
| Premium | `premium` | Best quality: Claude Opus 4.6, GPT-5.2 Codex |
| Free | `free` | Zero-cost models only (NVIDIA GPT-OSS 120B) |

**Configuration:**
```bash
export ROUTING_PROFILE=auto          # auto, eco, premium, free
export ROUTING_FORCE_AGENTIC=false   # force agentic tier selection
export ROUTING_SESSION_PINNING=true  # reuse model within a session
```

Or in `settings.json`:
```json
{
  "routing_profile": "auto",
  "routing_force_agentic": false,
  "routing_session_pinning": true
}
```

**Key features:**
- Session pinning: first request in a session determines the model; subsequent requests reuse it
- Rate-limit cooldown: models that return 429 are deprioritized for 60 seconds
- Agentic detection: tool-heavy requests (file ops, deploy, iterate) auto-upgrade to agentic tiers
- Cost estimation: every routing decision includes estimated cost and savings vs Claude Opus 4.5 baseline
- **Local-first**: Simple/Medium requests route to local Ollama models (qwen3-coder:30b, deepseek-r1:70b); Complex/Reasoning fall back to Claude Opus 4.6
- 22-model catalog across 8 providers (OpenAI, Anthropic, Google, DeepSeek, Moonshot, xAI, NVIDIA, Ollama)
- **Note**: GPT-4o, GPT-4.1, o4-mini were retired by OpenAI on 2026-02-13; GPT-5.3-Codex is the current SOTA coding model

### Deployment Topology

| Agent | Host | Role |
|-------|------|------|
| Frack | MacBook | Primary interactive agent (CLI/TUI, web gateway) |
| Frick | Homelab server (`alef`) | Production/infrastructure agent (shared PostgreSQL, K3s, Ollama) |

Both share PostgreSQL on `alef` and Logseq (synced natively). Both run Ollama bare metal for local inference.

### Roadmap

Full plan: [`docs/INTEGRATION_PLAN.md`](docs/INTEGRATION_PLAN.md). Remaining gaps: [`docs/REMAINING_INTEGRATION_WORK.md`](docs/REMAINING_INTEGRATION_WORK.md).

- **Phase 1 (Security)**: ✅ Done — command guard (20 packs), integrity (heartbeat-wired), WASM verification
- **Phase 2 (Compaction)**: ✅ Done — 5-stage pipeline (observations, dedup, dictionary, patterns, text opt) + salience scoring, wired into compaction
- **Phase 3 (OpenClaw gaps)**: ✅ Done — BOOT.md on startup, memory flush with tool execution loop (max 3 iterations), daily session reset
- **Phase 4 (Skills)**: ✅ Done — 97 bundled skills from 25 reference repos, multi-source discovery
- **Phase 4b (Router)**: ✅ Done — Intelligent LLM router (15-dimension classifier, 4 profiles, 22-model catalog, local-first routing, Opus 4.6 cloud fallback)
- **Phase 5 (Advanced)**: ✅ Done — weave-core + auto-merge, monty + external functions, task graph + memory decay
- **Phase 5.5 (LLM Providers)**: ✅ Done — Gemini backend added, Ollama default updated to qwen3-coder:30b, 6 total backends
- **Phase 6 (Database MCP)**: [genai-toolbox](https://github.com/googleapis/genai-toolbox) MCP Toolbox for Databases — structured DB tools via MCP for PostgreSQL + future data sources
- **Phase 6b (Session Intelligence)**: ✅ Done — learnings system (PostgreSQL + 3 LLM tools + prompt injection), salience scoring (turn/session importance), cross-machine session merge (content-hash dedup)
- **Phase 7+**: Frick deployment, multi-agent shared workspace, enhanced Logseq sync

### Reference Repo Integration Process

To add a new reference repository:

1. `git clone --depth 1 <url> examples/reference-repos/<short-name>`
2. Assess: read skills, source, README — what's novel vs already covered?
3. Adopt: copy skills to `skills/`, port algorithms to Rust, or document patterns
4. Document: update `docs/INTEGRATION_PLAN.md`, `FEATURE_PARITY.md`, `CLAUDE.md`, `README.md`
5. Do not add as Cargo dependency — vendor or port instead
