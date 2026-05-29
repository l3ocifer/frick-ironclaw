#!/usr/bin/env bash
set -euo pipefail

echo "=== IronClaw Frick (Homelab K3s) Setup ==="

# This script runs FROM the MacBook, deploying to alef via ArgoCD.
# Prerequisites:
#   1. ghcr.io/l3ocifer/ironclaw:latest image exists (pushed by CI)
#   2. ArgoCD is running on alef with homelab-services app
#   3. ironclaw database exists in PostgreSQL
#   4. Sealed Secrets controller is installed

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Step 1: Verify database exists
echo "[1/5] Checking ironclaw database on alef..."
DB_EXISTS=$(ssh alef "kubectl exec -n databases deploy/postgres -- psql -U postgres -tAc \"SELECT 1 FROM pg_database WHERE datname='ironclaw'\"" 2>/dev/null)
if [ "$DB_EXISTS" != "1" ]; then
    echo "  Creating ironclaw database..."
    ssh alef "kubectl exec -n databases deploy/postgres -- psql -U postgres -c 'CREATE DATABASE ironclaw;'"
    ssh alef "kubectl exec -n databases deploy/postgres -- psql -U postgres -c \"CREATE USER ironclaw WITH PASSWORD 'CHANGEME'; GRANT ALL PRIVILEGES ON DATABASE ironclaw TO ironclaw;\""
    echo "  Database created. UPDATE THE PASSWORD in sealed secrets!"
else
    echo "  Database exists."
fi

# Step 2: Verify pgvector extension
echo "[2/5] Ensuring pgvector extension..."
ssh alef "kubectl exec -n databases deploy/postgres -- psql -U postgres -d ironclaw -c 'CREATE EXTENSION IF NOT EXISTS vector;'" 2>/dev/null

# Step 3: Verify kustomize build
echo "[3/5] Validating kustomize manifests..."
cd "$PROJECT_DIR"
if command -v kustomize &>/dev/null; then
    kustomize build k8s/overlays/homelab > /dev/null
    echo "  Manifests valid."
elif command -v kubectl &>/dev/null; then
    kubectl kustomize k8s/overlays/homelab > /dev/null
    echo "  Manifests valid."
else
    echo "  WARN: kustomize/kubectl not available locally, skipping validation."
fi

# Step 4: Create namespace and secrets on cluster
echo "[4/5] Ensuring namespace and secrets..."
ssh alef "kubectl create namespace ironclaw --dry-run=client -o yaml | kubectl apply -f -" 2>/dev/null
echo "  Namespace ready."
echo ""
echo "  IMPORTANT: Create the sealed secret manually:"
echo "    ssh alef"
echo "    kubectl create secret generic ironclaw-secrets \\"
echo "      --namespace ironclaw \\"
echo "      --from-literal=DATABASE_URL='postgres://ironclaw:REAL_PASSWORD@external-postgres:5432/ironclaw' \\"
echo "      --from-literal=ANTHROPIC_API_KEY='sk-ant-...' \\"
echo "      --from-literal=OPENAI_API_KEY='sk-...' \\"
echo "      --from-literal=GEMINI_API_KEY='...' \\"
echo "      --dry-run=client -o yaml | \\"
echo "    kubeseal --controller-namespace sealed-secrets --format yaml > /tmp/ironclaw-sealed.yaml"
echo "    kubectl apply -f /tmp/ironclaw-sealed.yaml"
echo ""

# Step 5: ArgoCD sync
echo "[5/5] ArgoCD will auto-sync from production-apps.yaml."
echo "  If ironclaw entry is in production-apps.yaml and pushed to homelab repo, ArgoCD handles the rest."
echo "  To force sync: argocd app sync ironclaw"

echo ""
echo "=== Frick Setup Summary ==="
echo "  Image:     ghcr.io/l3ocifer/ironclaw:latest"
echo "  Namespace: ironclaw"
echo "  DB:        ironclaw on postgres.databases.svc.cluster.local"
echo "  Ollama:    host-ollama.ai.svc.cluster.local:11434"
echo "  MCP:       mcp-server.ai.svc.cluster.local:8890"
echo "  Ingress:   ironclaw.leopaska.xyz"
echo ""
echo "After deploying, verify:"
echo "  ssh alef 'kubectl get pods -n ironclaw'"
echo "  curl https://ironclaw.leopaska.xyz/health"
