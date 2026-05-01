# Frick — IronClaw cluster-ops agent

This is **Leo's fork of [nearai/ironclaw](https://github.com/nearai/ironclaw)**,
extended to run as `Frick` (the agent that owns the homelab cluster:
infra observability, deploys, ArgoCD syncs, GPU/thermal management on
alef, code work) inside [Leo's homelab K3s cluster](https://github.com/l3ocifer/homelab).

## Layout

```
frick-ironclaw/                      ← repo root (this fork)
├── (upstream ironclaw source)
│   ├── src/
│   ├── migrations/
│   ├── skills/
│   ├── wit/
│   ├── Cargo.toml
│   └── ...
└── homelab/                          ← everything we add
    ├── Dockerfile                    ← multi-stage Rust build
    ├── k8s/                          ← deployment, pvc, svc, ingress, rbac
    ├── config/                       ← SOUL.md, TOOLS.md, openclaw.json
    ├── shared/                       ← submodule → l3ocifer/homelab
    ├── .github/workflows/
    └── PATCHES.md, CHANGELOG.md, README.md
```

## Frick's persona, in 30 seconds

The infrastructure operator. Owns the K3s cluster, ArgoCD, monitoring,
the alef GPU thermal envelope, and most code work. Talks like an
SRE — terse, precise, gives Leo numbers not feelings. See
`config/SOUL.md`.

## Required env vars

Provided by `frick-secrets` SealedSecret in `agents-shared`. See
`config/openclaw.json` for the full reference.

## License

IronClaw upstream: see [LICENSE-MIT / LICENSE-APACHE](../LICENSE-MIT)
at repo root. Homelab additions in `homelab/`: same.
Persona text in `config/SOUL.md` is Leo Paska's IP.
