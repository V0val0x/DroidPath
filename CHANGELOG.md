# Changelog

All notable changes to **DroidPath** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.2.1] - 2026-11-22

### Added
- **MCP `ToolAnnotations` & 100% Schema Coverage:** Added `McpToolAnnotations` (`title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) across all 9 MCP tools, documented every parameter with explicit coordinate units/constraints/defaults (`android_swipe` raised from 33% to 100%), removed internal jargon, and added explicit sibling usage guidance and return/error contracts.
- **Glama Registry Container & GitHub Security Posture:** Added multi-stage `Dockerfile` and `.dockerignore`, Dependabot vulnerability/update configuration (`.github/dependabot.yml`), CodeQL static analysis workflow (`.github/workflows/codeql.yml`), and full GitHub Community Standards (`CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, `ACCESSIBILITY.md`, issue & PR templates).

### Fixed
- **MCP `initialize` Protocol Negotiation:** Negotiates requested `protocolVersion` across `2024-11-05`, `2025-03-26`, and `2025-06-18` so both legacy and strict MCP 2025 inspectors complete handshake cleanly.

## [2.2.0] - 2026-11-22

### Added
- **Cross-Device Zero-PII Android Settings Seed Graph (`SeedGraphCatalog`) & Seed-to-Real Hydration:** Pre-populates new graphs with 20 canonical `com.android.settings` screens and 19 deterministic edges (EN/RU + Pixel/AOSP/OneUI/HyperOS/OxygenOS semantic aliases, zero coordinates, zero PII) that automatically hydrate into exact device hashes, selectors, and coordinates on first live traversal (opt-out via `DROIDPATH_DISABLE_SEED_GRAPH=1`). 100% bit-exact `shadow_graph.json` interoperability with AndOnMac `v2.5.0`.
- **Fail-Closed SHA-256 Supply-Chain Verification:** Release workflow now publishes `checksums.txt` for all binaries, desktop bundles, installers, and `android-daemon.jar`; `scripts/install.sh`, `scripts/install.ps1`, and `packages/droidpath-cli` verify SHA-256 fail-closed with 2-hop GitHub → external S3 CDN redirect pinning and atomic installation.
- **Desktop HUD Seed Graph Styling:** Live SVG Spatial Graph Explorer visually distinguishes seeded vs. hydrated vertices and edges, with telemetry counters (`seed_vertex_count`, `hydrated_vertex_count`).
- **Expanded Native Installer MCP Client Auto-Configuration:** `--configure-mcp` (`install.sh`) and `-ConfigureMcp` (`install.ps1`) now support `gemini` and `antigravity` in addition to `cursor`, `claude`, and `all`.
- **Windows CI Runtime Verification for `install.ps1`:** Added real `windows-latest` CI test invoking `powershell.exe -File scripts/install.ps1` against a 2-hop GitHub → S3 CDN mock server (`application/octet-stream` `checksums.txt`, redirect pinning, MCP config generation, and fail-closed tamper rejection).
- **MCP Ecosystem & Registry Readiness:** Added official MCP Registry `server.json`, `glama.json`, `SECURITY.md`, `CHANGELOG.md`, and complete copy-paste configs for Claude Desktop, Cursor, Claude Code, VS Code / Copilot, Gemini CLI, Antigravity, and LM Studio / HTTP SSE.

### Security
- **Strict RFC 6454 Loopback HTTP/SSE Hardening:** Enforced exact loopback `Origin` and `Host` validation plus `Content-Length` enforcement on `droidpath-mcp --http` (`127.0.0.1:8766`).

### Changed
- Reordered README onboarding to lead with 100% working-today native GitHub Release installers (`install.sh` / `install.ps1`) and direct `droidpath-mcp` binary MCP configs, clearly documenting local-clone usage for `packages/droidpath-cli`.

## [2.1.1] - 2026-11-21

### Fixed
- Resolved `ClientSession` resync deadlock in `android-daemon` and guarded snapshot enqueues against losing control frames.
- Added `InputInjector` and TCP cancellation guards plus cold-start `hello` wait synchronization.
- Handled Windows loopback TCP SYN delay in `wait_for_hello` test and cleaned up Clippy lints.

## [2.1.0] - 2026-11-21

### Added
- Native `UiAutomation` input injection (`tap`, `swipe`, `keyevent`) directly over daemon TCP `:27189` with zero-fling 120Hz swipes and automatic `adb` fallback.
- Dynamic cold-start exploration (`android_explore_and_click` and `auto_explore: true`), Luhn `<CARD>` and `<OTP>` PII scrubbing, `Back-to-Root` drift recovery, `Smart Scroll-to-Selector`, `consecutiveFailures` edge decay, and HITL / `FLAG_SECURE` safety gates.
- Unified 2.0s 1x presentation showcase GIF across documentation.

## [2.0.0] - 2026-11-19

### Added
- Cross-platform zero-JVM Rust core (`droidpath-core`) and standalone native MCP CLI (`droidpath-mcp`) for Windows, Linux, and macOS.
- Cross-platform Tauri v2 Desktop HUD (`apps/droidpath-desktop`) with embedded `scrcpy-server` H.264 → WebCodecs mirroring, 3-color Agentic HUD, interactive HITL controls, and live SVG Spatial Graph Explorer.
- Node.js CLI (`packages/droidpath-cli`) and bundled LLM onboarding rules (`.cursorrules`, `CLAUDE.md`, `GEMINI.md`, `docs/system_prompt.md`).

## [1.0.2] - 2026-11-15

### Fixed
- Added swipe fling quiescence and updated showcase HUD visuals.

## [1.0.1] - 2026-11-15

### Fixed
- Hardened HTTP/SSE transport, POSIX shell quoting, and multi-predicate tap matching.

## [1.0.0] - 2026-11-15

### Added
- Initial release of DroidPath: headless Kotlin Shadow DOM daemon (`android-daemon.jar`), persistent Spatial Graph engine, Layer 3 A* symbolic UI router, and MCP server bridge.
