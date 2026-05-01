# Changelog

Frick-IronClaw releases.

## Unreleased

### Added

- Initial homelab/ overlay scaffolding for the IronClaw runtime
- Dockerfile builds IronClaw + tilth from local fork source (mirrors
  upstream's Dockerfile structure for clean upstream syncs)
- Fresh k8s manifests (deployment, pvc, service, ingressroute, rbac)
  for `agents-shared` namespace + floating with soft-prefer alef
  (RTX 3090 routing) — replaces the previous "deferred to upstream
  overlay" pattern
- config/{SOUL,TOOLS}.md + openclaw.json for the cluster-ops persona
- ClusterRole `frick-cluster-ops`: read-everything, write-restricted
  (pod restarts, deployment patches, ArgoCD app syncs; no secrets)
- GitHub Actions: build.yml + upstream-sync.yml
- Submodule of l3ocifer/homelab at homelab/shared/
