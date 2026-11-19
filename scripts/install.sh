#!/usr/bin/env bash
# DroidPath Zero-Friction 1-Click Installer (macOS & Linux — zero Node.js required)
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.sh | bash
#   ./scripts/install.sh [--deploy] [--configure-mcp cursor|claude|all] [--from-local <dir>] [--version <tag>]
set -euo pipefail

REPO="V0val0x/DroidPath"
INSTALL_ROOT="${DROIDPATH_HOME:-$HOME/.droidpath}"
BIN_DIR="$INSTALL_ROOT/bin"
SHARE_DIR="$INSTALL_ROOT/share"
VERSION="latest"
FROM_LOCAL=""
DO_DEPLOY=0
CONFIGURE_MCP=""
DAEMON_PORT=27189
SERIAL=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:?Missing value for --version}"; shift 2 ;;
    --from-local) FROM_LOCAL="${2:?Missing value for --from-local}"; shift 2 ;;
    --deploy) DO_DEPLOY=1; shift ;;
    --configure-mcp)
      if [[ $# -ge 2 && "${2:-}" != --* ]]; then
        CONFIGURE_MCP="$2"; shift 2
      else
        CONFIGURE_MCP="all"; shift
      fi
      ;;
    --daemon-port) DAEMON_PORT="${2:?Missing value for --daemon-port}"; shift 2 ;;
    --serial) SERIAL="${2:?Missing value for --serial}"; shift 2 ;;
    -h|--help)
      cat <<EOF
DroidPath Zero-Friction Installer (macOS & Linux, no Node.js required)

OPTIONS:
  --version <tag>               Release tag to install (default: latest)
  --from-local <repo_dir>       Install from local Cargo build + android-daemon build
  --deploy                      Auto-inject android-daemon.jar onto connected Android device
  --configure-mcp [target]      Auto-configure MCP client: cursor | claude | all
  --daemon-port <port>          Shadow DOM daemon port (default: 27189)
  --serial <id>                 Target ADB serial for --deploy
EOF
      exit 0
      ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ ! "$DAEMON_PORT" =~ ^[0-9]+$ ]] || (( DAEMON_PORT < 1 || DAEMON_PORT > 65535 )); then
  echo "ERROR: Invalid --daemon-port '$DAEMON_PORT' (expected 1..65535)" >&2
  exit 2
fi

if [[ -n "$CONFIGURE_MCP" && "$CONFIGURE_MCP" != "cursor" && "$CONFIGURE_MCP" != "claude" && "$CONFIGURE_MCP" != "all" ]]; then
  echo "ERROR: Invalid --configure-mcp target '$CONFIGURE_MCP' (expected cursor, claude, or all)" >&2
  exit 2
fi

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS/$ARCH" in
  Darwin/arm64)  TARGET="aarch64-apple-darwin" ;;
  Darwin/x86_64) TARGET="x86_64-apple-darwin" ;;
  Linux/x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64|Linux/arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) echo "ERROR: Unsupported platform $OS/$ARCH" >&2; exit 1 ;;
esac

mkdir -p "$BIN_DIR" "$SHARE_DIR"
MCP_DEST="$BIN_DIR/droidpath-mcp"
JAR_DEST="$SHARE_DIR/android-daemon.jar"

if [[ -n "$FROM_LOCAL" ]]; then
  LOCAL_BIN="$FROM_LOCAL/target/release/droidpath-mcp"
  [[ -f "$LOCAL_BIN" ]] || LOCAL_BIN="$FROM_LOCAL/target/debug/droidpath-mcp"
  LOCAL_JAR="$FROM_LOCAL/android-daemon/build/dist/android-daemon.jar"
  [[ -f "$LOCAL_JAR" ]] || LOCAL_JAR="$FROM_LOCAL/packages/droidpath-cli/assets/android-daemon.jar"

  if [[ ! -f "$LOCAL_BIN" ]]; then
    echo "ERROR: Local droidpath-mcp binary not found in $FROM_LOCAL/target/{release,debug}" >&2
    exit 1
  fi
  if [[ ! -f "$LOCAL_JAR" ]]; then
    echo "ERROR: Local android-daemon.jar not found in $FROM_LOCAL" >&2
    exit 1
  fi
  cp "$LOCAL_BIN" "$MCP_DEST"
  cp "$LOCAL_JAR" "$JAR_DEST"
  chmod +x "$MCP_DEST"
else
  if [[ "$VERSION" == "latest" ]]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
  else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
  fi
  echo "Downloading droidpath-mcp ($TARGET)..."
  curl -fsSL "$BASE_URL/droidpath-mcp-$TARGET" -o "$MCP_DEST"
  chmod +x "$MCP_DEST"
  echo "Downloading android-daemon.jar..."
  curl -fsSL "$BASE_URL/android-daemon.jar" -o "$JAR_DEST"
fi

echo "✅ Installed droidpath-mcp -> $MCP_DEST"
echo "✅ Installed android-daemon.jar -> $JAR_DEST"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
  echo "ℹ️  Add DroidPath to your PATH:"
  echo "   export PATH=\"$BIN_DIR:\$PATH\""
fi

configure_json_file() {
  local config_path="$1"
  mkdir -p "$(dirname "$config_path")"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$config_path" "$MCP_DEST" "$JAR_DEST" "$DAEMON_PORT" <<'PYEOF'
import json, os, sys
cfg_path, mcp_bin, jar_path, port = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
doc = {}
if os.path.exists(cfg_path):
    with open(cfg_path, "r", encoding="utf-8") as f:
        raw = f.read().strip()
        if raw:
            doc = json.loads(raw)
if not isinstance(doc, dict):
    raise SystemExit(f"Invalid JSON object in {cfg_path}")
servers = doc.get("mcpServers")
if servers is None:
    servers = {}
    doc["mcpServers"] = servers
elif not isinstance(servers, dict):
    raise SystemExit(f"Invalid 'mcpServers' object in {cfg_path}")
args = ["--stdio", "--daemon-jar", jar_path]
if port != 27189:
    args.extend(["--daemon-port", str(port)])
servers["droidpath"] = {"command": mcp_bin, "args": args}
tmp = cfg_path + ".tmp"
with open(tmp, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
os.replace(tmp, cfg_path)
PYEOF
    echo "✅ Configured MCP client -> $config_path"
  else
    echo "⚠️  python3 not found; please add droidpath manually to $config_path:"
    echo "   {\"mcpServers\":{\"droidpath\":{\"command\":\"$MCP_DEST\",\"args\":[\"--stdio\",\"--daemon-jar\",\"$JAR_DEST\"]}}}"
  fi
}

if [[ -n "$CONFIGURE_MCP" ]]; then
  if [[ "$CONFIGURE_MCP" == "cursor" || "$CONFIGURE_MCP" == "all" ]]; then
    configure_json_file "$HOME/.cursor/mcp.json"
  fi
  if [[ "$CONFIGURE_MCP" == "claude" || "$CONFIGURE_MCP" == "all" ]]; then
    if [[ "$OS" == "Darwin" ]]; then
      configure_json_file "$HOME/Library/Application Support/Claude/claude_desktop_config.json"
    else
      configure_json_file "$HOME/.config/Claude/claude_desktop_config.json"
    fi
  fi
fi

if [[ "$DO_DEPLOY" -eq 1 ]]; then
  DEPLOY_ARGS=("deploy" "--daemon-port" "$DAEMON_PORT" "--daemon-jar" "$JAR_DEST")
  [[ -n "$SERIAL" ]] && DEPLOY_ARGS+=("--serial" "$SERIAL")
  "$MCP_DEST" "${DEPLOY_ARGS[@]}"
fi
