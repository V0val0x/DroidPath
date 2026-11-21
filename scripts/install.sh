#!/usr/bin/env bash
# DroidPath Zero-Friction 1-Click Installer (macOS & Linux — zero Node.js required)
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.sh | bash
#   ./scripts/install.sh [--deploy] [--configure-mcp cursor|claude|gemini|antigravity|all] [--from-local <dir>] [--version <tag>]
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
ALLOW_UNVERIFIED="${DROIDPATH_ALLOW_UNVERIFIED:-0}"

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
    --allow-unverified) ALLOW_UNVERIFIED=1; shift ;;
    -h|--help)
      cat <<EOF
DroidPath Zero-Friction Installer (macOS & Linux, no Node.js required)

OPTIONS:
  --version <tag>               Release tag to install (default: latest)
  --from-local <repo_dir>       Install from local Cargo build + android-daemon build
  --deploy                      Auto-inject android-daemon.jar onto connected Android device
  --configure-mcp [target]      Auto-configure MCP client: cursor | claude | gemini | antigravity | all
  --daemon-port <port>          Shadow DOM daemon port (default: 27189)
  --serial <id>                 Target ADB serial for --deploy
  --allow-unverified            Skip SHA-256 checksums.txt verification (unsafe)
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

if [[ -n "$CONFIGURE_MCP" && "$CONFIGURE_MCP" != "cursor" && "$CONFIGURE_MCP" != "claude" && "$CONFIGURE_MCP" != "gemini" && "$CONFIGURE_MCP" != "antigravity" && "$CONFIGURE_MCP" != "all" ]]; then
  echo "ERROR: Invalid --configure-mcp target '$CONFIGURE_MCP' (expected cursor, claude, gemini, antigravity, or all)" >&2
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

compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -- "$file" | awk '{print tolower($1)}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -- "$file" | awk '{print tolower($1)}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | awk '{print tolower($NF)}'
  else
    echo "ERROR: No SHA-256 tool found (need sha256sum, shasum, or openssl)." >&2
    return 1
  fi
}

lookup_checksum() {
  local manifest="$1"
  local asset="$2"
  awk -v target="$asset" '
    {
      sub(/\r$/, "")
      if ($0 ~ /^[[:space:]]*#/ || $0 ~ /^[[:space:]]*$/) next
      if (match($0, /^[0-9A-Fa-f]{64}[[:space:]]+\*?.+$/)) {
        hash = tolower(substr($0, 1, 64))
        rest = substr($0, 65)
        sub(/^[[:space:]]+\*?/, "", rest)
        sub(/^\.\//, "", rest)
        if (rest == target) {
          print hash
          exit 0
        }
      }
    }
  ' "$manifest"
}

verify_asset_sha256() {
  local file="$1"
  local manifest="$2"
  local asset="$3"
  local expected
  expected="$(lookup_checksum "$manifest" "$asset")"
  if [[ -z "$expected" ]]; then
    echo "ERROR: Asset '$asset' is missing from checksums.txt. Refusing unverified install (pass --allow-unverified to override)." >&2
    return 1
  fi
  local actual
  actual="$(compute_sha256 "$file")"
  if [[ "$actual" != "$expected" ]]; then
    echo "ERROR: SHA-256 mismatch for '$asset'!" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    echo "Refusing to install tampered or corrupted artifact." >&2
    return 1
  fi
  echo "✅ Verified SHA-256 for $asset (${actual:0:16}...)"
}

resolve_pinned_base_url() {
  local initial_base="$1"
  local headers_file="$2"
  local effective_url="$3"
  awk -v initial_base="$initial_base" -v effective_url="$effective_url" '
    function origin_of(u,    m, rest, slash) {
      if (match(u, /^[A-Za-z][A-Za-z0-9+.-]*:\/\/[^\/?#]+/)) {
        return substr(u, RSTART, RLENGTH)
      }
      return ""
    }
    function strip_qf(u) {
      sub(/[?#].*$/, "", u)
      return u
    }
    function try_pin(candidate,    clean, c_origin) {
      if (candidate == "") return
      if (substr(candidate, 1, 1) == "/") {
        candidate = init_origin candidate
      }
      clean = strip_qf(candidate)
      c_origin = origin_of(clean)
      if (c_origin == init_origin && clean ~ /\/checksums\.txt$/) {
        sub(/\/checksums\.txt$/, "", clean)
        pinned = clean
      }
    }
    BEGIN {
      init_origin = origin_of(initial_base)
      pinned = initial_base
    }
    {
      sub(/\r$/, "")
      if (tolower($0) ~ /^location:[[:space:]]*/) {
        loc = $0
        sub(/^[^:]+:[[:space:]]*/, "", loc)
        sub(/[[:space:]]+$/, "", loc)
        try_pin(loc)
      }
    }
    END {
      try_pin(effective_url)
      print pinned
    }
  ' "$headers_file"
}

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
  if [[ -n "${DROIDPATH_RELEASE_BASE_URL:-}" ]]; then
    BASE_URL="${DROIDPATH_RELEASE_BASE_URL%/}"
  elif [[ "$VERSION" == "latest" ]]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
  else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
  fi

  TMP_STAGE="$(mktemp -d)"
  trap 'rm -rf "${TMP_STAGE:-}"' EXIT

  MCP_ASSET="droidpath-mcp-$TARGET"
  JAR_ASSET="android-daemon.jar"

  if [[ "$ALLOW_UNVERIFIED" != "1" ]]; then
    echo "Fetching SHA-256 checksums.txt..."
    if ! EFFECTIVE_URL="$(curl -fsSL -D "$TMP_STAGE/checksums.headers" -o "$TMP_STAGE/checksums.txt" -w '%{url_effective}' "$BASE_URL/checksums.txt")"; then
      echo "ERROR: Failed to fetch SHA-256 manifest ($BASE_URL/checksums.txt). Refusing unverified download (pass --allow-unverified or set DROIDPATH_ALLOW_UNVERIFIED=1 to override)." >&2
      exit 1
    fi
    BASE_URL="$(resolve_pinned_base_url "$BASE_URL" "$TMP_STAGE/checksums.headers" "$EFFECTIVE_URL")"
  else
    echo "⚠️  SHA-256 verification bypassed (--allow-unverified / DROIDPATH_ALLOW_UNVERIFIED=1)." >&2
  fi

  echo "Downloading $MCP_ASSET..."
  curl -fsSL "$BASE_URL/$MCP_ASSET" -o "$TMP_STAGE/$MCP_ASSET"

  echo "Downloading $JAR_ASSET..."
  curl -fsSL "$BASE_URL/$JAR_ASSET" -o "$TMP_STAGE/$JAR_ASSET"

  if [[ "$ALLOW_UNVERIFIED" != "1" ]]; then
    verify_asset_sha256 "$TMP_STAGE/$MCP_ASSET" "$TMP_STAGE/checksums.txt" "$MCP_ASSET"
    verify_asset_sha256 "$TMP_STAGE/$JAR_ASSET" "$TMP_STAGE/checksums.txt" "$JAR_ASSET"
  fi

  chmod +x "$TMP_STAGE/$MCP_ASSET"
  mv -f "$TMP_STAGE/$MCP_ASSET" "$MCP_DEST"
  mv -f "$TMP_STAGE/$JAR_ASSET" "$JAR_DEST"
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
  if [[ "$CONFIGURE_MCP" == "gemini" || "$CONFIGURE_MCP" == "all" ]]; then
    configure_json_file "$HOME/.gemini/settings.json"
  fi
  if [[ "$CONFIGURE_MCP" == "antigravity" || "$CONFIGURE_MCP" == "all" ]]; then
    configure_json_file "$HOME/.gemini/antigravity/mcp_config.json"
  fi
fi

if [[ "$DO_DEPLOY" -eq 1 ]]; then
  DEPLOY_ARGS=("deploy" "--daemon-port" "$DAEMON_PORT" "--daemon-jar" "$JAR_DEST")
  [[ -n "$SERIAL" ]] && DEPLOY_ARGS+=("--serial" "$SERIAL")
  "$MCP_DEST" "${DEPLOY_ARGS[@]}"
fi
