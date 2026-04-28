<p align="center">
  <img src="ironclaw.png" alt="IronClaw" width="200"/>
</p>

<h1 align="center">IronClaw</h1>

<p align="center">
  <strong>Your secure personal AI assistant, always on your side</strong>
</p>

<p align="center">
  <a href="#philosophy">Philosophy</a> •
  <a href="#features">Features</a> •
  <a href="#installation">Installation</a> •
  <a href="#configuration">Configuration</a> •
  <a href="#security">Security</a> •
  <a href="#architecture">Architecture</a>
</p>

---

## Philosophy

IronClaw is built on a simple principle: **your AI assistant should work for you, not against you**.

In a world where AI systems are increasingly opaque about data handling and aligned with corporate interests, IronClaw takes a different approach:

- **Your data stays yours** - All information is stored locally, encrypted, and never leaves your control
- **Transparency by design** - Open source, auditable, no hidden telemetry or data harvesting
- **Self-expanding capabilities** - Build new tools on the fly without waiting for vendor updates
- **Defense in depth** - Multiple security layers protect against prompt injection and data exfiltration

IronClaw is the AI assistant you can actually trust with your personal and professional life.

## Features

### Security First

- **WASM Sandbox** - Untrusted tools run in isolated WebAssembly containers with capability-based permissions
- **Credential Protection** - Secrets are never exposed to tools; injected at the host boundary with leak detection
- **Prompt Injection Defense** - Pattern detection, content sanitization, and policy enforcement
- **Endpoint Allowlisting** - HTTP requests only to explicitly approved hosts and paths

### Always Available

- **Multi-channel** - REPL, HTTP webhooks, WASM channels (Telegram, Slack), and web gateway
- **Docker Sandbox** - Isolated container execution with per-job tokens and orchestrator/worker pattern
- **Web Gateway** - Browser UI with real-time SSE/WebSocket streaming
- **Routines** - Cron schedules, event triggers, webhook handlers for background automation
- **Heartbeat System** - Proactive background execution for monitoring and maintenance tasks
- **Parallel Jobs** - Handle multiple requests concurrently with isolated contexts
- **Self-repair** - Automatic detection and recovery of stuck operations

### Self-Expanding

- **Dynamic Tool Building** - Describe what you need, and IronClaw builds it as a WASM tool
- **MCP Protocol** - Connect to Model Context Protocol servers for additional capabilities
- **Plugin Architecture** - Drop in new WASM tools and channels without restarting
- **Agent Skills** - 97 bundled skills with progressive loading (only name + description in context; full instructions on demand)
- **Sandboxed Python** - Execute Python code safely via [monty](https://github.com/pydantic/monty) with resource limits (no I/O, no network)

### Intelligent LLM Routing

- **15-Dimension Classifier** - Weighted scoring across token count, code presence, reasoning markers, technical terms, agentic task detection, and more
- **Local-First** - Routes Simple/Medium requests to local Ollama models; Complex/Reasoning falls back to cloud (Claude Opus 4.6)
- **4 Profiles** - Auto (with agentic detection), Eco, Premium, Free
- **Session Pinning** - Reuses model within a session; rate-limit cooldown for failed providers
- **22-Model Catalog** - OpenAI, Anthropic, Google, DeepSeek, Moonshot, xAI, NVIDIA, Ollama

### Persistent Memory

- **Hybrid Search** - Full-text + vector search using Reciprocal Rank Fusion
- **Workspace Filesystem** - Flexible path-based storage for notes, logs, and context
- **Identity Files** - Maintain consistent personality and preferences across sessions
- **Semantic Merge** - Entity-level 3-way merge via [weave-core](https://github.com/Ataraxy-Labs/weave) for concurrent multi-agent edits
- **Token Compression** - 5-stage deterministic pipeline (observations, dedup, dictionary, patterns, text opt)

### Multi-Agent Coordination

- **Task Graph** - PostgreSQL-backed DAG for task dependencies, priorities, and assignment across agents
- **Agent Identity** - Per-agent `AGENT_ID` for task scoping and workspace isolation
- **JSONL Export** - Beads-compatible task export/import for interoperability
- **Memory Decay** - Automatic archival of old completed tasks

## Installation

### Prerequisites

- Rust 1.90+
- PostgreSQL 15+ with [pgvector](https://github.com/pgvector/pgvector) extension
- NEAR AI account (authentication handled via setup wizard)

## Download or Build

Visit [Releases page](https://github.com/nearai/ironclaw/releases/) to see the latest updates.

<details>
  <summary>Install via Windows Installer (Windows)</summary>

Download the [Windows Installer](https://github.com/nearai/ironclaw/releases/latest/download/ironclaw-x86_64-pc-windows-msvc.msi) and run it.

</details>

<details>
  <summary>Install via powershell script (Windows)</summary>

```sh
irm https://github.com/nearai/ironclaw/releases/latest/download/ironclaw-installer.ps1 | iex
```

</details>

<details>
  <summary>Install via shell script (macOS, Linux, Windows/WSL)</summary>

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nearai/ironclaw/releases/latest/download/ironclaw-installer.sh | sh
```
</details>

<details>
  <summary>Compile the source code (Cargo on Windows, Linux, macOS)</summary>

Install it with `cargo`, just make sure you have [Rust](https://rustup.rs) installed on your computer.

```bash
# Clone the repository
git clone https://github.com/nearai/ironclaw.git
cd ironclaw

# Build
cargo build --release

# Run tests
cargo test
```

For **full release** (after modifying channel sources), run `./scripts/build-all.sh` to rebuild channels first.

</details>

### Database Setup

```bash
# Create database
createdb ironclaw

# Enable pgvector
psql ironclaw -c "CREATE EXTENSION IF NOT EXISTS vector;"
```

## Configuration

Run the setup wizard to configure IronClaw:

```bash
ironclaw onboard
```

The wizard handles database connection, NEAR AI authentication (via browser OAuth),
and secrets encryption (using your system keychain). All settings are saved to
`~/.ironclaw/settings.json`.

### Multi-Agent Deployment

To run multiple agents sharing the same PostgreSQL, set a unique `AGENT_ID` per instance:

```bash
# MacBook (Frack)
AGENT_NAME=Frack AGENT_ID=frack ironclaw run

# Homelab (Frick)
AGENT_NAME=Frick AGENT_ID=frick ironclaw run
```

Agents share the same task graph and workspace, isolated by `(user_id, agent_id)`. See `.env.example` for all configuration options.

## Security

IronClaw implements defense in depth to protect your data and prevent misuse.

### WASM Sandbox

All untrusted tools run in isolated WebAssembly containers:

- **Capability-based permissions** - Explicit opt-in for HTTP, secrets, tool invocation
- **Endpoint allowlisting** - HTTP requests only to approved hosts/paths
- **Credential injection** - Secrets injected at host boundary, never exposed to WASM code
- **Leak detection** - Scans requests and responses for secret exfiltration attempts
- **Rate limiting** - Per-tool request limits to prevent abuse
- **Resource limits** - Memory, CPU, and execution time constraints

```
WASM ──► Allowlist ──► Leak Scan ──► Credential ──► Execute ──► Leak Scan ──► WASM
         Validator     (request)     Injector       Request     (response)
```

### Prompt Injection Defense

External content passes through multiple security layers:

- Pattern-based detection of injection attempts
- Content sanitization and escaping
- Policy rules with severity levels (Block/Warn/Review/Sanitize)
- Tool output wrapping for safe LLM context injection

### Data Protection

- All data stored locally in your PostgreSQL database
- Secrets encrypted with AES-256-GCM
- No telemetry, analytics, or data sharing
- Full audit log of all tool executions

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│                          Channels                              │
│  ┌──────┐  ┌──────┐   ┌─────────────┐  ┌─────────────┐         │
│  │ REPL │  │ HTTP │   │WASM Channels│  │ Web Gateway │         │
│  └──┬───┘  └──┬───┘   └──────┬──────┘  │ (SSE + WS)  │         │
│     │         │              │         └──────┬──────┘         │
│     └─────────┴──────────────┴────────────────┘                │
│                              │                                 │
│                    ┌─────────▼─────────┐                       │
│                    │    Agent Loop     │  Intent routing       │
│                    └────┬──────────┬───┘                       │
│                         │          │                           │
│              ┌──────────▼────┐  ┌──▼───────────────┐           │
│              │  Scheduler    │  │ Routines Engine  │           │
│              │(parallel jobs)│  │(cron, event, wh) │           │
│              └──────┬────────┘  └────────┬─────────┘           │
│                     │                    │                     │
│       ┌─────────────┼────────────────────┘                     │
│       │             │                                          │
│   ┌───▼─────┐  ┌────▼────────────────┐                         │
│   │ Local   │  │    Orchestrator     │                         │
│   │Workers  │  │  ┌───────────────┐  │                         │
│   │(in-proc)│  │  │ Docker Sandbox│  │                         │
│   └───┬─────┘  │  │   Containers  │  │                         │
│       │        │  │ ┌───────────┐ │  │                         │
│       │        │  │ │Worker / CC│ │  │                         │
│       │        │  │ └───────────┘ │  │                         │
│       │        │  └───────────────┘  │                         │
│       │        └─────────┬───────────┘                         │
│       └──────────────────┤                                     │
│                          │                                     │
│              ┌───────────▼──────────┐                          │
│              │    Tool Registry     │                          │
│              │  Built-in, MCP, WASM │                          │
│              └──────────────────────┘                          │
└────────────────────────────────────────────────────────────────┘
```

### Core Components

| Component | Purpose |
|-----------|---------|
| **Agent Loop** | Main message handling and job coordination |
| **Router** | Classifies user intent (command, query, task) |
| **Scheduler** | Manages parallel job execution with priorities |
| **Worker** | Executes jobs with LLM reasoning and tool calls |
| **Orchestrator** | Container lifecycle, LLM proxying, per-job auth |
| **Web Gateway** | Browser UI with chat, memory, jobs, logs, extensions, routines |
| **Routines Engine** | Scheduled (cron) and reactive (event, webhook) background tasks |
| **Workspace** | Persistent memory with hybrid search |
| **Safety Layer** | Prompt injection defense and content sanitization |

## Usage

```bash
# First-time setup (configures database, auth, etc.)
ironclaw onboard

# Start interactive REPL
cargo run

# With debug logging
RUST_LOG=ironclaw=debug cargo run
```

## Development

```bash
# Format code
cargo fmt

# Lint
cargo clippy --all --benches --tests --examples --all-features

# Run tests
createdb ironclaw_test
cargo test

# Run specific test
cargo test test_name
```

- **Telegram channel**: See [docs/TELEGRAM_SETUP.md](docs/TELEGRAM_SETUP.md) for setup and DM pairing.
- **Changing channel sources**: Run `./channels-src/telegram/build.sh` before `cargo build` so the updated WASM is bundled.

## LLM Provider Configuration

IronClaw supports 6 LLM backends with local-first routing. Set `LLM_BACKEND` env var or `llm_backend` in settings.

| Backend | Value | Default Model | Notes |
|---------|-------|---------------|-------|
| NEAR AI | `nearai` | `llama4-maverick-instruct-basic` | Session or API key auth |
| Ollama (local) | `ollama` | `qwen3-coder:30b` | Zero cost, runs on Frack/Frick |
| OpenAI | `openai` | `gpt-5.3-codex` | GPT-4o/4.1/o4-mini retired Feb 2026 |
| Anthropic | `anthropic` | `claude-opus-4.6` | 1M context (beta), 128K output |
| Gemini | `gemini` | `gemini-2.5-pro` | 1M+ context |
| OpenAI-compatible | `openai_compatible` | `default` | Any endpoint speaking OpenAI API |

The intelligent router (`ROUTING_PROFILE=auto|eco|premium|free`) automatically classifies requests and routes to the optimal model. Local Ollama models handle simple/medium tasks; cloud models (Opus 4.6) handle complex/reasoning tasks.

## Reference Repositories

IronClaw draws on 24 reference repositories for algorithms, patterns, and skills. These are cloned into `examples/reference-repos/` for analysis — they are **not** runtime dependencies.

### Current Reference Repos

| Repository | Directory | What Was Adopted |
|-----------|-----------|------------------|
| [dcg](https://github.com/Dicklesworthstone/destructive_command_guard) | `dcg/` | Command guard — 20 security packs ported to Rust |
| [claw-compactor](https://github.com/aeromomo/claw-compactor) | `claw-compactor/` | 5-stage token compression pipeline ported to Rust |
| [clawsec](https://github.com/prompt-security/clawsec) | `clawsec/` | Workspace integrity monitor (SHA-256 drift detection) |
| [weave](https://github.com/Ataraxy-Labs/weave) | `weave/` | Vendored `weave-core` for semantic 3-way merge |
| [monty](https://github.com/pydantic/monty) | `monty/` | Sandboxed Python interpreter with external function bridge |
| [beads](https://github.com/steveyegge/beads) | `beads/` | Task dependency graph concepts + memory decay |
| [aline](https://github.com/human-re/GCC) | `aline/` | Git-backed memory patterns (reference) |
| [OneContext](https://github.com/TheAgentContextLab/OneContext) | `OneContext/` | Multi-agent context handoff patterns (reference) |
| [ClawRouter](https://github.com/BlockRunAI/ClawRouter) | `ClawRouter/` | Intelligent LLM router (15-dim classifier) fully ported to Rust |
| [pi-skills](https://github.com/badlogic/pi-skills) | `pi-skills/` | 4 skills adopted (brave-search, gccli, gdcli, youtube-transcript) |
| [ralph](https://github.com/snarktank/ralph) | `ralph/` | PRD-driven patterns → prd-generator skill |
| [compound-product](https://github.com/snarktank/compound-product) | `compound-product/` | Report analysis → report-analyzer, task-breakdown skills |
| [pIRS](https://github.com/nickslevine/pIRS) | `pIRS/` | Tool usage analytics patterns (reference) |
| [homeassistant-skill](https://github.com/komal-SkyNET/claude-skill-homeassistant) | `homeassistant-skill/` | Home Assistant skill copied |
| [arscontexta](https://github.com/agenticnotetaking/arscontexta) | `arscontexta/` | 10 knowledge management skills copied |
| [next-plaid](https://github.com/lightonai/next-plaid) | `next-plaid/` | ColGREP semantic code search skill + multi-vector patterns (reference) |
| [solana-dev-skill](https://github.com/solana-foundation/solana-dev-skill) | `solana-dev-skill/` | Solana development skill with 10 sub-docs |
| [gemini-skills](https://github.com/google-gemini/gemini-skills) | `gemini-skills/` | Gemini API development skill |
| [webgpu-skill](https://github.com/dgreenheck/webgpu-claude-skill) | `webgpu-skill/` | WebGPU/Three.js/TSL skill with docs + examples |
| [asc-skills](https://github.com/rudrankriyam/app-store-connect-cli-skills) | `asc-skills/` | 13 App Store Connect automation skills |
| [compound-engineering](https://github.com/EveryInc/compound-engineering-plugin) | `compound-engineering/` | Coding tutor + engineering skills |
| [youtube-clipper](https://github.com/op7418/Youtube-clipper-skill) | `youtube-clipper/` | Video processing pipeline skill |
| [kiss_ai](https://github.com/ksenxx/kiss_ai) | `kiss_ai/` | Evolutionary optimization patterns (reference) |
| [genai-toolbox](https://github.com/googleapis/genai-toolbox) | `genai-toolbox/` | MCP Toolbox for Databases — structured DB tools via MCP |
| [contrail](https://github.com/strangeloopcanon/contrail) | `contrail/` | Session flight recorder — learnings system, salience scoring, context packs, cross-machine merge |

### Adding a New Reference Repository

Follow this process to evaluate and integrate a new reference repo:

1. **Clone** into `examples/reference-repos/`:
   ```bash
   cd examples/reference-repos
   git clone --depth 1 <repo-url> <short-name>
   ```

2. **Assess** — Read all SKILL.md files, READMEs, and source code. Determine:
   - Does it provide skills, algorithms, or architectural patterns?
   - Is anything already covered by IronClaw's existing capabilities?
   - What is the effort to integrate (trivial skill copy vs Rust port)?

3. **Adopt** what's useful:
   - **Skills** (SKILL.md format): Copy to `skills/<name>/` with IronClaw-compatible frontmatter
   - **Algorithms**: Port to Rust in the appropriate `src/` module
   - **Patterns**: Document in `docs/INTEGRATION_PLAN.md` for future reference

4. **Document** the assessment:
   - Add a section in `docs/INTEGRATION_PLAN.md` (section 2.x)
   - Update `FEATURE_PARITY.md` if new capabilities were added
   - Update `CLAUDE.md` project context
   - Update this README's reference repos table

5. **Do not** add the reference repo as a Cargo dependency — it's reference material only. If code needs to be used, vendor it or port it to Rust.

## OpenClaw Heritage

IronClaw is a Rust reimplementation inspired by [OpenClaw](https://github.com/openclaw/openclaw). See [FEATURE_PARITY.md](FEATURE_PARITY.md) for the complete tracking matrix and [CLAUDE.md](CLAUDE.md) for full project context.

Key differences:

- **Rust vs TypeScript** - Native performance, memory safety, single binary
- **WASM sandbox vs Docker** - Lightweight, capability-based security
- **PostgreSQL vs SQLite** - Production-ready persistence
- **Security-first design** - Multiple defense layers, credential protection
- **Intelligent router** - 15-dimension classifier with local-first model routing
- **97 bundled skills** - From 24 reference repos, auto-discovered
- **Token compression** - 5-stage deterministic pipeline

### Memory Features (from OpenClaw)

- **Session save on `/new`** - Current conversation saved to `daily/` before starting a new thread
- **Pre-compaction memory flush** - Silent LLM turn before compaction to persist durable notes
- **Main-session-only MEMORY.md** - Long-term memory excluded from group/shared contexts for privacy
- **Logseq integration** - Personal knowledge graph injected into agent context at bootstrap
- **Recommended AGENTS.md template** - Operating instructions for memory-first agent behavior

## Documentation

| Document | Purpose |
|----------|---------|
| [CLAUDE.md](CLAUDE.md) | Full project context for AI assistants (architecture, config, security, roadmap) |
| [FEATURE_PARITY.md](FEATURE_PARITY.md) | OpenClaw ↔ IronClaw feature tracking matrix |
| [AGENTS.md](AGENTS.md) | Agent operating rules |
| [docs/INTEGRATION_PLAN.md](docs/INTEGRATION_PLAN.md) | Reference repo integration plan + roadmap (24 repos assessed) |
| [docs/REMAINING_INTEGRATION_WORK.md](docs/REMAINING_INTEGRATION_WORK.md) | Detailed remaining tasks |
| [docs/MEMORY_TRANSFER_PLAN.md](docs/MEMORY_TRANSFER_PLAN.md) | OpenClaw → IronClaw memory feature transfer plan |
| [docs/reference/AGENTS.recommended.md](docs/reference/AGENTS.recommended.md) | Recommended AGENTS.md template for workspace setup |
| [docs/TELEGRAM_SETUP.md](docs/TELEGRAM_SETUP.md) | Telegram channel setup and DM pairing |

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.


<!-- homelab-deployment:begin -->
## Homelab Deployment

This service is deployed to production on the [`l3ocifer/homelab`](https://github.com/l3ocifer/homelab) K3s cluster via ArgoCD GitOps. **Future engineers should treat the homelab repo as the operational source of truth.**

### Cluster footprint

| Field | Value |
|---|---|
| **ArgoCD Application** | [`ironclaw`](https://argocd.leopaska.xyz/applications/ironclaw) |
| **AppSet entry** | [`argocd/apps/production-apps.yaml`](https://github.com/l3ocifer/homelab/blob/main/argocd/apps/production-apps.yaml) |
| **Namespace** | `ironclaw` |
| **Public URL** | https://ironclaw.leopaska.xyz |
| **Manifest path (this repo)** | `k8s/overlays/homelab/` |
| **Tracked branch** | `main` |
| **Container images** | `ghcr.io/l3ocifer/ironclaw:latest` |
| **Image auto-update** | ArgoCD Image Updater (newest-build strategy) |

### Required platform resources (provided by homelab)

| Resource | Endpoint / Reference |
|---|---|
| **Postgres (with pgvector)** | `homelab-pg-rw.databases.svc.cluster.local:5432` — DB `ironclaw` created via `postInitSQL` in [`argocd/apps/_postgres/homelab-pg.yaml`](https://github.com/l3ocifer/homelab/blob/main/argocd/apps/_postgres/homelab-pg.yaml). pgvector extension is preloaded on `template1`. |
| **TLS / DNS** | Cloudflare Tunnel + Traefik IngressRoute, cert via cert-manager (Let's Encrypt DNS-01) |

### SealedSecrets (committed to homelab repo)

| Secret | Type | Keys | Vaultwarden item |
|---|---|---|---|
| `ironclaw/ironclaw-secrets` | Opaque | (intentionally not enumerated — see Vaultwarden) | `ironclaw-bundle` |

### Re-seal procedure

If a secret reports `no key could decrypt secret (...)` after a cluster rebuild:

1. Plaintext source of truth: self-hosted Vaultwarden at https://warden.leopaska.xyz
2. Follow the step-by-step re-seal flow: [`docs/argocd-triage.md#re-seal-procedure-per-secret`](https://github.com/l3ocifer/homelab/blob/main/docs/argocd-triage.md#re-seal-procedure-per-secret)
3. For the GHCR pull secret use the bulk loop in [`docs/argocd-triage.md#ghcr-pull-secrets-special-case--dockerconfigjson`](https://github.com/l3ocifer/homelab/blob/main/docs/argocd-triage.md#ghcr-pull-secrets-special-case--dockerconfigjson)

### Image build & deploy flow

1. Push to `main` triggers GitHub Actions build (`.github/workflows/`)
2. Image pushed to GHCR
3. ArgoCD Image Updater (in `argocd` ns of homelab cluster) detects new digest within ~2m
4. ArgoCD applies the updated manifest; rolling restart

### Operational references

- **Live status**: https://argocd.leopaska.xyz/applications/ironclaw
- **Logs**: https://grafana.leopaska.xyz → Explore → Loki → `{namespace="ironclaw"}`
- **Metrics**: kube-prometheus-stack scrapes pods/services with `prometheus.io/scrape: "true"`
- **Production apps catalog**: [`docs/production-apps.md`](https://github.com/l3ocifer/homelab/blob/main/docs/production-apps.md)
- **Disaster recovery runbook**: [`docs/disaster-recovery.md`](https://github.com/l3ocifer/homelab/blob/main/docs/disaster-recovery.md)
- **Secrets workflow**: [`docs/secrets-checklist.md`](https://github.com/l3ocifer/homelab/blob/main/docs/secrets-checklist.md)
- **Health triage**: [`docs/argocd-triage.md`](https://github.com/l3ocifer/homelab/blob/main/docs/argocd-triage.md)
<!-- homelab-deployment:end -->
