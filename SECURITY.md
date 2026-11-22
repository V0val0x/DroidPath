# Security Policy

## Supported Versions

| Version | Supported |
| :--- | :---: |
| `2.3.x` | ✅ Active |
| `2.2.x` | ⚠️ Upgrade to `2.3.x` |
| `< 2.2.0` | ❌ Unsupported |

## Built-In Security & Privacy Guarantees

DroidPath is engineered for local-first, privacy-preserving Android automation:

- **Zero-PII On-Device & On-Disk:**
  - `android-daemon` masks `isPassword` / sensitive editable fields to `***` on-device before transmitting any frame over TCP `:27189`.
  - `ScreenLayoutHasher` and `SpatialGraphEngine` scrub Luhn-validated payment card PANs (`<CARD>`) and contextual 2FA/OTP codes (`<OTP>`) before hashing or persisting `~/.droidpath/shadow_graph.json`.
  - The pre-populated Android Settings Seed Graph (`SeedGraphCatalog`) contains zero device identifiers, zero coordinates, and zero user data.
- **Strict Loopback Network Surface (`127.0.0.1`) & Auth-Gated Container Binds:**
  - By default, `android-daemon` (`127.0.0.1:27189` or dynamic `--daemon-port 0` via `adb forward`) and `droidpath-mcp --http` (`127.0.0.1:8766`) bind exclusively to loopback. Any non-loopback `--bind-host` / `DROIDPATH_BIND_HOST` (for headless Docker fleets) is strictly refused at startup unless a non-empty `Authorization: Bearer <token>` is configured via `DROIDPATH_AUTH_TOKEN` or `--auth-token`.
  - The HTTP/SSE MCP server validates RFC 6454 loopback `Origin` and `Host` headers (mitigating DNS rebinding and browser-based CSRF) and enforces strict `Content-Length` bounds.
- **Mandatory Human-in-the-Loop (HITL) & Biometric Safety Gates:**
  - Hazardous or irreversible actions (payments, purchases, transfers, deletions, factory resets) halt with `status: "requires_confirmation"` until explicitly approved with `confirmed: true`.
  - Screens marked `FLAG_SECURE` or showing biometric prompts immediately trigger `SECURE_SCREEN_ACTIVE`, pause automation until unlocked, strictly block opt-in Hybrid Vision (`include_vision: true`) screenshot capture, and suppress UI candidate/delta payloads in failure feedback.
- **Fail-Closed Supply-Chain Verification (SHA-256):**
  - Every GitHub Release publishes a `checksums.txt` SHA-256 manifest covering all binaries, installers, and `android-daemon.jar`.
  - `scripts/install.sh`, `scripts/install.ps1`, and `packages/droidpath-cli` pin the GitHub release tag across 2-hop CDN redirects, verify SHA-256 digests fail-closed, and install atomically.

## Reporting a Vulnerability

Please **do not** report security vulnerabilities in public GitHub issues.

1. Use **GitHub Private Vulnerability Reporting** via the repository's **Security → Advisories → Report a vulnerability** page:
   https://github.com/V0val0x/DroidPath/security/advisories/new
2. Include reproduction steps, affected version/platform, and potential impact.
3. We aim to acknowledge reports within **72 hours** and coordinate disclosure once a fix and verified release are ready.
