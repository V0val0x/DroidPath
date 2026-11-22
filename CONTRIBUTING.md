# Contributing to DroidPath

Thank you for your interest in contributing to **DroidPath**! Whether you are fixing a bug, improving Shadow DOM stability, adding Seed Graph support, or polishing documentation, your help is welcome.

Please read our [Code of Conduct](./CODE_OF_CONDUCT.md) and [Security Policy](./SECURITY.md) before contributing.

---

## Development Environment & Prerequisites

- **Rust toolchain:** `stable` (`cargo`, `clippy`, `rustfmt`)
- **Node.js:** `18+` (for `packages/droidpath-cli` and the `apps/droidpath-desktop` Vite/TypeScript frontend)
- **Android Platform Tools:** `adb` in `PATH` (needed for live device testing)
- **JDK 17 & Android SDK:** Required **only** if modifying or rebuilding `android-daemon/`
- **Linux Desktop HUD build deps (Ubuntu/Debian):** `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`

---

## Building & Running Tests

Run the full verification suite locally before opening a Pull Request:

```bash
# 1. Rust workspace linter & unit/integration tests (droidpath-core, droidpath-mcp, droidpath-desktop)
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Node.js 1-click CLI tests
npm --prefix packages/droidpath-cli test

# 3. Tauri v2 Desktop HUD frontend tests & TypeScript/Vite build
cd apps/droidpath-desktop && npm install && npm test && npm run build && cd ../..

# 4. Kotlin headless daemon tests & single-DEX JAR rebuild (if android-daemon/ changed)
./android-daemon/scripts/build-dex.sh
npm --prefix packages/droidpath-cli run sync-daemon

# 5. Installer shell syntax check (if scripts/install.sh changed)
bash -n scripts/install.sh
```

---

## Critical Architectural Invariants

To keep **DroidPath** (Windows/Linux/macOS) and its native macOS companion [**AndOnMac**](https://github.com/V0val0x/AndOnMac) 100% interoperable, please respect these contracts:

1. **`android-daemon/src` Parity:** Changes inside `android-daemon/src` MUST be synced across both repositories (`diff -rq DroidPath/android-daemon/src AndOnMac/android-daemon/src` must be empty), and `packages/droidpath-cli/assets/android-daemon.jar` must be rebuilt with `build-dex.sh` + `sync-daemon`.
2. **`shadow_graph.json` Binary & JSON Schema Compatibility:** Layout hashing (64-bit FNV-1a in `ScreenLayoutHasher`), field names (camelCase with custom alternating-array `spanningChildren` / ISO-8601 UTC seconds / uppercase UUIDs), and Seed Graph canonical anchors must stay compatible between Rust `droidpath-core` and Swift `AndroidShadowGraphKit`.
3. **Privacy by Design & HITL Safety:**
   - Never log or persist unmasked passwords, payment card PANs (`<CARD>` via Luhn), or 2FA/OTP codes (`<OTP>`).
   - Keep HTTP/SSE listeners strictly loopback-bound (`127.0.0.1`) with strict `Host` and RFC 6454 `Origin` validation.
   - Preserve `requires_confirmation` gates on hazardous/irreversible actions and `FLAG_SECURE` biometric handoff pauses.
4. **No PII in Seed Graphs or Fixtures:** Any test fixture or Seed Graph update must be completely synthetic and free of personal account names, device serials, or private app data.

---

## Pull Request Workflow

1. **Fork** the repository and create a branch from `main` (e.g., `fix/scroll-settle` or `feat/oem-seed-aliases`).
2. **Keep diffs focused:** Avoid unrelated formatting churn across untouched files.
3. **Add tests:** Include unit or integration tests covering bug fixes and new behaviors (especially across Windows/Linux/macOS path handling, PII redaction, or routing recovery).
4. **Update documentation:** Reflect user-facing CLI flags, MCP tools, or config changes in `README.md` and `README_RU.md`.
5. **Submit the PR** using the [Pull Request Template](./.github/PULL_REQUEST_TEMPLATE.md) and confirm GitHub Actions CI passes on all OS targets (`ubuntu-latest`, `macos-15`, `windows-latest`, Kotlin daemon).

---

## Reporting Bugs & Suggesting Features

- For security vulnerabilities, please **do not** open a public issue — use [GitHub Security Advisories](https://github.com/V0val0x/DroidPath/security/advisories/new) as described in [SECURITY.md](./SECURITY.md).
- For bugs and enhancements, use our structured [Issue Templates](https://github.com/V0val0x/DroidPath/issues/new/choose).
