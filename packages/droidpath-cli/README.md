# droidpath

Zero-friction CLI & MCP launcher for **[DroidPath](https://github.com/V0val0x/DroidPath)** — autonomous Android UI control powered by an on-device `<1ms` Shadow DOM daemon, persistent A* Spatial Graph router, Zero-PII Cross-Device Settings Seed Graph with automatic Seed-to-Real hydration, and mandatory privacy/HITL safety gates.

## Recommended Installation (Works Today via GitHub Releases)

If you do not want to clone the repo or use Node.js, install the native Rust `droidpath-mcp` binary and `android-daemon.jar` directly from GitHub Releases with fail-closed SHA-256 verification:

**macOS & Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.sh | bash -s -- --configure-mcp all
# Add --deploy if an Android device is already attached over USB/Wi-Fi.
```

**Windows (PowerShell):**
```powershell
$env:DROIDPATH_CONFIGURE_MCP="all"; irm https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.ps1 | iex
# Add $env:DROIDPATH_DEPLOY="1"; if an Android device is already attached over USB/Wi-Fi.
```

## Running the Node.js CLI (Local Clone / `npm link`)

> **Note:** `droidpath` is not published to `npmjs.org` yet. Run it from a local clone or via `npm link` (`npx -y droidpath ...` will work once published to npm).

```bash
git clone https://github.com/V0val0x/DroidPath.git
cd DroidPath

# Option 1: Run directly from clone
node packages/droidpath-cli/bin/droidpath.mjs start --configure

# Option 2: Link globally on your machine
npm --prefix packages/droidpath-cli link

# 1. Deploy the bundled android-daemon.jar (USB or Wi-Fi ADB, no APK install)
droidpath start

# 2. Auto-configure MCP & agent rules for Cursor, Claude, Gemini CLI, Antigravity, LM Studio & Ollama
droidpath init

# 3. Launch the native Rust MCP server (stdio or HTTP/SSE)
droidpath mcp --stdio
droidpath mcp --http --port 8766

# 4. Run diagnostics
droidpath doctor
```

## Copy-Paste MCP Configuration (Direct Native Binary)

### Claude Desktop / Cursor / Gemini CLI / Antigravity (`stdio`)

```json
{
  "mcpServers": {
    "droidpath": {
      "command": "/Users/<you>/.droidpath/bin/droidpath-mcp",
      "args": ["--stdio", "--daemon-jar", "/Users/<you>/.droidpath/share/android-daemon.jar"]
    }
  }
}
```

### VS Code / GitHub Copilot (`.vscode/mcp.json`)

```json
{
  "servers": {
    "droidpath": {
      "type": "stdio",
      "command": "droidpath-mcp",
      "args": ["--stdio"]
    }
  }
}
```

## Security & Supply-Chain Integrity

- **SHA-256 Verified Downloads**: Prebuilt `droidpath-mcp` binaries downloaded from GitHub Releases are verified fail-closed against the release `checksums.txt` manifest with 2-hop CDN redirect pinning before atomic installation.
- **127.0.0.1 Loopback Only**: Both the Android Shadow DOM forward (`127.0.0.1:27189`) and local MCP HTTP/SSE server (`127.0.0.1:8766`) bind strictly to loopback with RFC 6454 `Origin`/`Host` validation and `Content-Length` enforcement.
- **Privacy & Safety**: Password fields are masked to `***` on-device, PANs/OTPs are scrubbed (`<CARD>`/`<OTP>`) before hashing or graph persistence, `FLAG_SECURE`/biometric screens pause automation, and hazardous actions (`Pay`, `Delete`, `Transfer`, `Reset`) require explicit Human-in-the-Loop confirmation (`confirmed: true`).

## Links

- **GitHub**: https://github.com/V0val0x/DroidPath
- **macOS Native Companion**: https://github.com/V0val0x/AndOnMac
- **Security Policy**: https://github.com/V0val0x/DroidPath/blob/main/SECURITY.md
- **License**: MIT
