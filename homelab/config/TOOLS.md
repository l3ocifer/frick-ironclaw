# TOOLS.md - Frick's Environment

*This is the homelab. My domain.*

## This Machine: Alef Server

| Spec | Value |
|------|-------|
| **Hostname** | alef |
| **CPU** | Intel Core i7-3820 @ 3.60GHz (4 cores, 8 threads) |
| **RAM** | 54GB |
| **GPU** | NVIDIA GeForce RTX 3090 (24GB VRAM) |
| **OS** | Ubuntu 22.04 LTS |
| **Local IP** | 192.168.1.200 |

### Storage

| Mount | Size | Purpose |
|-------|------|---------|
| `/` (root) | 232GB | System and k8s |
| `/media/l3o/prod` | 3.7TB | Primary data, Docker volumes |
| `/media/l3o/archive` | 5.5TB | Backups, media archive |

### GPU Power Management

**CRITICAL:** The RTX 3090 is power-limited to prevent thermal shutdown.

| Setting | Value |
|---------|-------|
| **Current Power Limit** | 115W |
| **Default Power Limit** | 350W |
| **Percentage of Default** | ~33% |
| **Reason** | Prevents GPU overheating, which causes CPU thermal throttling and system shutdown |

The i7-3820 is a 2012-era CPU sharing airflow with a modern 350W GPU. At full power, the GPU heats the case enough to trigger CPU thermal protection. The 115W cap keeps everything stable.

```bash
# Check GPU status
nvidia-smi
```

**Do not remove the power limit** without understanding the thermal implications.

## Ollama Models (Server)

Models installed on alef:

| Model | Size | Best For |
|-------|------|----------|
| `qwen2.5-coder:32b` | 19GB | Code generation (primary) |
| `qwen3-coder:30b` | 18GB | Code completion |
| `gemma3:27b` | 17GB | General reasoning |
| `mistral-small3.2:24b` | 15GB | Fast general use |
| `codestral:22b` | 12GB | Code-focused Mistral |
| `deepcoder:14b` | 9GB | Lightweight code |
| `deepseek-coder-v2:16b` | 8.9GB | Code with reasoning |
| `phi3.5:latest` | 2.2GB | Fast, small tasks |

```bash
ollama list                    # See all models
ollama ps                      # Currently loaded
ollama run qwen2.5-coder:32b   # Run primary model
```

Note: Server models differ from MacBook due to VRAM vs unified memory constraints. The RTX 3090's 24GB VRAM can load larger Q8 quantizations.

## Kubernetes (k3s) - Complete Service List

**83 pods** across 20 namespaces. Here's every single one:

### Namespace: `ai` (5 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| librechat | Running | Alternative chat UI |
| litellm-proxy | Running | Multi-model API gateway |
| mcp-server | Running | Model Context Protocol server |
| open-webui | Running | Primary chat interface |
| qdrant | Running | Vector database for embeddings |

Services: `host-ollama`, `librechat`, `litellm-proxy`, `mcp-server`, `ollama`, `open-webui`, `qdrant`, `comfyui`, `exo`

### Namespace: `ae` (5 pods - American Angel)

| Pod | Replicas | Status |
|-----|----------|--------|
| american-angel | 5 | Running |

Services: `american-angel-service`, `external-logto`, `external-postgres`, `external-redis`

### Namespace: `argocd` (8 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| argocd-application-controller | Running | Watches git repos |
| argocd-applicationset-controller | Running | ApplicationSet CRD |
| argocd-dex-server | Running | OIDC/SSO |
| argocd-image-updater | Running | Auto-updates images |
| argocd-notifications-controller | Running | Notifications |
| argocd-redis | Running | Cache |
| argocd-repo-server | Running | Git operations |
| argocd-server | Running | API/UI server |

### Namespace: `authorworks` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| authorworks-server | Running | Backend API |
| authorworks-frontend | **CreateContainerConfigError** ⚠️ | Frontend (broken) |
| authorworks-book-generator | **CreateContainerConfigError** ⚠️ | Book gen (broken) |

### Namespace: `blink-platform` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| blink-platform | Running | theblink.live platform |

### Namespace: `blink-streaming` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| ovenmediaengine | Running | RTMP/WebRTC streaming |

LoadBalancer at 192.168.1.200 for streaming ports.

### Namespace: `cert-manager` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| cert-manager | Running | Certificate automation |
| cert-manager-cainjector | Running | CA injection |
| cert-manager-webhook | Running | Webhook validation |

### Namespace: `chimera` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| chimera | **CrashLoopBackOff** ⚠️ | AI image gen (271 restarts) |

### Namespace: `communication` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| conduit | Running | Matrix homeserver |
| element | Running | Matrix web client |
| rustpad | Running | Collaborative editing |

### Namespace: `databases` (6 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| postgres | Running | Primary PostgreSQL |
| redis | Running | Cache/queue |
| mongodb | Running | Document store |
| rabbitmq | Running | Message queue |
| pgadmin | Running | PostgreSQL UI |
| whodb | Running | Database explorer |

### Namespace: `githired` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| githired | Running | githired.work platform |

### Namespace: `hyvapaska` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| hyvapaska-app | Running | hyvapaska.com |

### Namespace: `ingress-system` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| cloudflared (x2) | Running | Cloudflare Tunnel (HA) |
| traefik | Running | Ingress controller |

### Namespace: `kube-system` (5 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| coredns | Running | DNS |
| local-path-provisioner | Running | PVC provisioning |
| metrics-server | Running | Resource metrics |
| sealed-secrets-controller | Running | Secret encryption |
| svclb-ome-external | Running (6/6) | LoadBalancer for streaming |

### Namespace: `lunasea` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| tanks-js | Running | lunasea.social game |

### Namespace: `media` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| jellyfin | Running | Media server |

### Namespace: `monitoring` (11 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| grafana | Running | Dashboards |
| homelab-dashboard | Running | Custom dashboard |
| loki | Running | Log aggregation |
| node-exporter | Running | System metrics |
| nvidia-gpu-exporter | Running | GPU metrics |
| prometheus | Running | Metrics database |
| tempo | Running | Distributed tracing |
| umami | Running | Privacy analytics |
| uptime-kuma | Running | Uptime monitoring |
| vector | Running | Log shipping |
| test-* | Completed | Test pods |

### Namespace: `omnilemma` (1 pod)

| Pod | Status | Purpose |
|-----|--------|---------|
| omnilemma-platform | Running | omnilemma.com |

### Namespace: `potluck` (2 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| potluck-backend | Running | API server |
| potluck-frontend | Running | React frontend |

### Namespace: `productivity` (7 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| guacamole | Running (2/2) | Remote desktop gateway |
| homeassistant | Running | Smart home |
| huginn | Running | Agent automation |
| mailhog | Running | Email testing |
| mosquitto | Running | MQTT broker |
| n8n | Running | Workflow automation |
| postiz | Running | Social media management |

### Namespace: `security` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| authelia | Running | SSO/2FA |
| logto | Running | Identity provider |
| vaultwarden | Running | Password manager |

### Namespace: `storage` (2 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| minio | Running | S3-compatible storage |
| syncthing | Running | File sync |

### Namespace: `trade` (2 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| api-gateway | Running | Trade bot API |
| frontend | Running | Trade bot UI |

### Namespace: `ursulai` (3 pods)

| Pod | Status | Purpose |
|-----|--------|---------|
| ursulai-backend | Running | API server |
| ursulai-celery-worker | Running | Async tasks |
| ursulai-frontend | Running | React frontend |

## Docker Containers

Only **1 container** running outside k3s:

| Container | Image | Status | Port |
|-----------|-------|--------|------|
| comfyui | ghcr.io/ai-dock/comfyui:latest-cuda | Up 18h | 8188 |

ComfyUI runs in Docker (not k3s) for direct GPU access.

## Ingress Routes (Public Domains)

All traffic through Cloudflare Tunnel:

| Domain | Service | Notes |
|--------|---------|-------|
| argocd.leopaska.xyz | ArgoCD | GitOps dashboard |
| author.works | AuthorWorks | Book platform |
| api.author.works | AuthorWorks API | Backend |
| theblink.live | Blink Platform | Streaming |
| stream.theblink.live | OvenMediaEngine | RTMP ingest |
| chimera.red | Chimera | Currently down |
| githired.work | GitHired | Job platform |
| hyvapaska.com | Hyvä Paska | Finnish humor |
| lunasea.social | Tanks JS | Game |
| omnilemma.com | Omnilemma | Philosophy |
| potluck.pub | Potluck | Social dining |
| ursulai.com | UrsulaAI | AI assistant |

## Internal Services (*.leopaska.xyz)

All accessible via Cloudflare Tunnel:

### AI & ML
| Service | URL | Purpose |
|---------|-----|---------|
| Open WebUI | `openwebui.leopaska.xyz` | Primary chat interface |
| Ollama | `ollama.leopaska.xyz` | Direct Ollama access |
| LiteLLM Admin | `llm-admin.leopaska.xyz` | LiteLLM proxy admin |
| LiteLLM API | `llm.leopaska.xyz` | Multi-model API gateway |
| LibreChat | `librechat.leopaska.xyz` | Alternative chat UI |
| ComfyUI | `comfyui.leopaska.xyz` | AI image generation |
| Exo | `exo.leopaska.xyz` | Distributed inference |
| Qdrant | `qdrant.leopaska.xyz` | Vector database |
| MCP Server | `mcp.leopaska.xyz` | Model Context Protocol |

### Communication
| Service | URL | Purpose |
|---------|-----|---------|
| Element | `element.leopaska.xyz` | Matrix web client |
| Conduit | `conduit.leopaska.xyz` | Matrix homeserver |
| Rustpad | `rustpad.leopaska.xyz` | Collaborative editor |

### Automation
| Service | URL | Purpose |
|---------|-----|---------|
| n8n | `workflow.leopaska.xyz` | Workflow automation |
| Huginn | `huginn.leopaska.xyz` | Agent automation |
| Home Assistant | `homeassistant.leopaska.xyz` | Smart home |
| Postiz | `postiz.leopaska.xyz` | Social media mgmt |
| MailHog | `mailhog.leopaska.xyz` | Email testing |
| MQTT | `mqtt.leopaska.xyz` | Mosquitto broker |

### Infrastructure
| Service | URL | Purpose |
|---------|-----|---------|
| ArgoCD | `argocd.leopaska.xyz` | GitOps dashboard |
| Traefik | `traefik.leopaska.xyz` | Ingress dashboard |
| Grafana | `grafana.leopaska.xyz` | Metrics dashboards |
| Prometheus | `prometheus.leopaska.xyz` | Raw metrics |
| Loki | `loki.leopaska.xyz` | Log aggregation |
| Tempo | `tempo.leopaska.xyz` | Distributed tracing |
| Uptime Kuma | `uptimekuma.leopaska.xyz` | Uptime monitoring |
| Umami | `umami.leopaska.xyz` | Privacy analytics |
| Dashboard | `dashboard.leopaska.xyz` | Homelab overview |
| SyncThing | `syncthing.leopaska.xyz` | File sync dashboard |

### Databases
| Service | URL | Purpose |
|---------|-----|---------|
| pgAdmin | `pgadmin.leopaska.xyz` | PostgreSQL admin |
| WhoDB | `whodb.leopaska.xyz` | Database explorer |
| RabbitMQ | `rabbitmq.leopaska.xyz` | Message queue admin |

### Security
| Service | URL | Purpose |
|---------|-----|---------|
| Vaultwarden | `warden.leopaska.xyz` | Password manager |
| Authelia | `authelia.leopaska.xyz` | SSO/2FA |
| Logto | `auth.leopaska.xyz` | Identity provider |
| Logto Admin | `auth-admin.leopaska.xyz` | Logto admin UI |

### Storage
| Service | URL | Purpose |
|---------|-----|---------|
| MinIO | `minio.leopaska.xyz` | S3-compatible storage |

### Media & Other
| Service | URL | Purpose |
|---------|-----|---------|
| Jellyfin | `jellyfin.leopaska.xyz` | Media server |
| Guacamole | `guacamole.leopaska.xyz` | Remote desktop gateway |

### Project Shortcuts
| Project | Internal URL | Public URL |
|---------|--------------|------------|
| American Angel | `ae.leopaska.xyz` | `americanangel.xyz` |
| Omnilemma | `omni.leopaska.xyz` | `omnilemma.com` |
| Potluck | `potluck.leopaska.xyz` | `potluck.pub` |

## File Locations

```
/home/l3o/git/
├── homelab/           # Infrastructure configs
├── ai/                # AI projects
└── production/        # Synced from MacBooks

/media/l3o/prod/       # Primary data drive (3.7TB)
├── docker/            # Docker volumes
│   ├── syncthing/
│   ├── postgres/
│   └── [others]
└── backups/

/media/l3o/archive/    # Archive drive (5.5TB)
```

## Common Commands

```bash
# Cluster health
kubectl get pods -A | grep -v Running    # Find problems
kubectl top nodes                         # Resource usage
nvidia-smi                                # GPU status

# Restart services
kubectl rollout restart deployment/<name> -n <namespace>

# Logs
kubectl logs -n <namespace> <pod> -f

# Database access
kubectl exec -it -n databases postgres-* -- psql -U postgres

# GPU monitoring (watch for thermals)
watch -n 2 nvidia-smi

# Disk usage (root is critical!)
df -h /
```

## Thermal Management

The system runs hot. The RTX 3090 and i7-3820 share a case with limited airflow.

| Component | Safe Range | Action if Exceeded |
|-----------|------------|-------------------|
| GPU | < 80°C | Power limit auto-throttles |
| CPU | < 90°C | System may shutdown |

The 115W power cap on the GPU is **not negotiable** without hardware changes.

---

*This file describes the environment. Keep it updated as things change.*
