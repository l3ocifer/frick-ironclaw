#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
PLIST_SRC="$SCRIPT_DIR/com.ironclaw.frack.plist"
PLIST_DST="$HOME/Library/LaunchAgents/com.ironclaw.frack.plist"

echo "=== IronClaw Frack (MacBook) Setup ==="

# 1. Build release binary
echo "[1/5] Building release binary..."
cd "$PROJECT_DIR"
cargo build --release

# 2. Ensure settings directory
echo "[2/5] Ensuring ~/.ironclaw/ exists..."
mkdir -p "$HOME/.ironclaw"

# 3. Ensure .env exists
if [ ! -f "$PROJECT_DIR/.env" ]; then
    echo "ERROR: $PROJECT_DIR/.env not found. Copy .env.example and configure."
    exit 1
fi

# 4. Install launchd plist
echo "[3/5] Installing launchd plist..."
if launchctl list 2>/dev/null | grep -q com.ironclaw.frack; then
    echo "  Unloading existing service..."
    launchctl unload "$PLIST_DST" 2>/dev/null || true
fi
cp "$PLIST_SRC" "$PLIST_DST"

# 5. Start the service
echo "[4/5] Loading service..."
launchctl load "$PLIST_DST"

echo "[5/5] Verifying..."
sleep 2
if launchctl list | grep -q com.ironclaw.frack; then
    echo "Frack is running. Logs: ~/.ironclaw/frack.log"
else
    echo "WARNING: Service may not have started. Check: launchctl list | grep ironclaw"
fi

echo ""
echo "=== Frack Setup Complete ==="
echo "  Binary:   $PROJECT_DIR/target/release/ironclaw"
echo "  Config:   $HOME/.ironclaw/settings.json"
echo "  Env:      $PROJECT_DIR/.env"
echo "  Logs:     $HOME/.ironclaw/frack.log"
echo "  Plist:    $PLIST_DST"
echo ""
echo "Commands:"
echo "  Stop:     launchctl unload ~/Library/LaunchAgents/com.ironclaw.frack.plist"
echo "  Start:    launchctl load ~/Library/LaunchAgents/com.ironclaw.frack.plist"
echo "  Logs:     tail -f ~/.ironclaw/frack.log"
