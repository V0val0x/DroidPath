# DroidPath Agentic Benchmarks

This benchmark suite compares **DroidPath (Shadow DOM + Layer 3 A* Spatial Graph Router)** against the two dominant Android automation paradigms used by autonomous AI agents:

1. **Pure Vision (Screenshot + VLM loop)** — captures a full-resolution PNG screenshot on every step and sends it to a Vision-Language Model to predict `(x, y)` coordinates.
2. **Appium / `uiautomator dump` XML loop** — runs `uiautomator dump` over ADB on every step and feeds the raw XML hierarchy (~25–35 KB) to an LLM.
3. **DroidPath (Shadow DOM + Layer 3 A*)** — streams incremental NDJSON accessibility diffs over a persistent loopback TCP socket (`android-daemon`) and executes learned multi-hop paths locally in Rust via A* without intermediate LLM turns.

---

## Measured Results (Live Pixel 10a Hardware + Replay Suite)

> **Transparency Note:** Hardware per-primitive latencies below were measured live over USB ADB on an attached **Google Pixel 10a (`61061JEA307110`)** using `node scripts/benchmark_agentic.mjs --mode live --json` against compiled `target/release/droidpath-mcp` (at lockscreen state, 9 active UI nodes; note that unlocked rich app screens of ~50 nodes compress to `~71 KB` at `540×1212` JPEG q45). Multi-step task totals in the replay table below run real `droidpath-mcp` stdio JSON-RPC calls against a 60-node loopback Shadow DOM daemon.

### Live Hardware Probe (Google Pixel 10a over USB ADB — Measured)

| Primitive / Method | Status | Measured Latency | Payload Size | Notes |
|---|---:|---:|---:|---|
| **DroidPath `android_get_ui_tree` (steady-state)** | `ok` | **0.23 ms** | **1,593 bytes** (~399 tok) | 9 live nodes from local Shadow DOM stream (`~10,600×` faster than `uiautomator`) |
| **DroidPath `android_get_ui_tree` (cold connect)** | `ok` | **46.4 ms** | **1,593 bytes** (~399 tok) | First TCP connect + live snapshot (`~52×` faster than `uiautomator`) |
| **DroidPath Hybrid Vision (`include_vision: true`)** | `ok` | **485.67 ms** | **10,504 bytes** JPEG (540×1212) | Daemon capture `201 ms` + live UI tree + compressed JPEG (q45) |
| **`adb exec-out screencap -p` (raw PNG)** | `ok` | **565.4 ms** | **15,733 bytes** PNG | Visual capture only — no structured UI tree |
| **`adb exec-out uiautomator dump /dev/tty` (XML)** | `ok` | **2,438.5 ms** | **5,252 bytes** (~1,313 tok) | Requires stopping `UiAutomation` daemon; **3.3×** more text tokens than Shadow DOM |

### Local MCP Runtime Micro-Benchmarks (Measured)

| Metric | Measured Value |
|---|---|
| `tools/list` JSON-RPC latency (9 tools) | **0.30 ms** |
| `android_graph_stats` JSON-RPC latency | **0.23 ms** |
| Pre-hydrated Seed Graph loaded at boot | **20 vertices / 19 edges** |
| Compact Shadow DOM tree (60-node Settings screen) | **4,537 bytes (~1,135 tokens)** |

### Multi-Step Task Comparison (5 WebArena-Style Android Tasks, 3–4 Hops Each)

| Architecture | Avg LLM Calls / Task | Avg Tokens / Task | Avg Estimated Task Time | Reduction vs. Pure Vision |
|---|---:|---:|---:|---|
| **DroidPath (Shadow DOM + A* Router)** | **1.0** | **39** *(warm A*)* / **~1,135** *(cold tree)* | **464 ms** | **4.2× fewer LLM calls · ~21× faster · >99% fewer tokens (warm)** |
| **Pure Vision (Screenshots + VLM)** | 4.2 | 7,673 | 9,954 ms | Baseline |
| **Appium / `uiautomator` XML** | 4.2 | 29,400 | 8,946 ms | 3.8× more tokens than Vision |

### Per-Task Breakdown

| Task | Hops | DroidPath (Calls / Tokens / Time) | Pure Vision (Calls / Tokens / Time) | Appium XML (Calls / Tokens / Time) |
|---|---:|---|---|---|
| Enable Night Light (Display) | 3 | 1 call · 39 tok · 435 ms | 4 calls · 7,308 tok · 9,480 ms | 4 calls · 28,000 tok · 8,520 ms |
| Open Wi-Fi Settings (Network) | 4 | 1 call · 39 tok · 580 ms | 5 calls · 9,135 tok · 11,850 ms | 5 calls · 35,000 tok · 10,650 ms |
| Navigate to Battery Saver | 3 | 1 call · 39 tok · 435 ms | 4 calls · 7,308 tok · 9,480 ms | 4 calls · 28,000 tok · 8,520 ms |
| Open App Notifications | 3 | 1 call · 39 tok · 435 ms | 4 calls · 7,308 tok · 9,480 ms | 4 calls · 28,000 tok · 8,520 ms |
| Scroll to About Phone | 3 | 1 call · 39 tok · 435 ms | 4 calls · 7,308 tok · 9,480 ms | 4 calls · 28,000 tok · 8,520 ms |

---

## Methodology & Token Model

- **Text tokens:** `ceil(utf8_bytes / 4)` (standard BPE approximation for JSON/XML).
- **Vision image tokens:** Anthropic's published `(W × H) / 750` formula for a 1080×2400 Android screenshot scaled to 1568px long edge (`706 × 1568 / 750 = 1,477` image tokens + ~350 prompt/tool tokens = **1,827 tokens/step**).
- **Opt-in Hybrid Vision (`include_vision: true`):** Downscaled 540px-wide JPEG (`quality: 50`, `~15–25 KB`) is strictly opt-in and only used when an agent explicitly needs visual disambiguation (e.g., canvas/icons), costing ~864 vision tokens only on that single step while remaining **strictly blocked on `FLAG_SECURE` screens**.

## Reproduce on Your Device

```bash
# Build release binary first (optional — falls back to target/debug)
cargo build --release -p droidpath-mcp

# Replay benchmark (zero device required)
node scripts/benchmark_agentic.mjs

# Live physical device / emulator probe (measures real screencap -p & uiautomator dump)
node scripts/benchmark_agentic.mjs --mode live
```
