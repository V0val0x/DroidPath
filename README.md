# DroidPath
<!-- mcp-name: io.github.V0val0x/droidpath -->

**High-Performance Infrastructure & API for Autonomous Android AI Agents**

*Sub-millisecond Shadow DOM Streaming · Persistent A\* Symbolic UI Router · Standardized OpenAI/Anthropic/MCP Tool Schemas · Opt-In Hybrid Vision · Headless Docker-Android Fleets*

[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](./LICENSE)
[![Platforms: Windows · Linux · macOS · Android](https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS%20%7C%20Android-blue.svg)](https://github.com/V0val0x/DroidPath)
[![GitHub Release](https://img.shields.io/github/v/release/V0val0x/DroidPath?color=brightgreen)](https://github.com/V0val0x/DroidPath/releases)
[![CI](https://github.com/V0val0x/DroidPath/actions/workflows/ci.yml/badge.svg)](https://github.com/V0val0x/DroidPath/actions/workflows/ci.yml)
[![V0val0x/DroidPath MCP server](https://glama.ai/mcp/servers/V0val0x/DroidPath/badges/score.svg)](https://glama.ai/mcp/servers/V0val0x/DroidPath)

**English** | [Русская версия (Russian)](README_RU.md) · [Agent Integrations](docs/integrations.md) · [Benchmarks](docs/benchmarks.md) · [Headless Docker Fleets](docs/headless-docker.md) · [Security](SECURITY.md) · [Changelog](CHANGELOG.md)

![DroidPath Autonomous Android Agent Infrastructure Demo](./assets/demo-droidpath-presentation.gif)

---

## Why DroidPath?

Traditional Android automation tools were built for brittle QA scripts, and pure Vision-Language Model (VLM) loops burn thousands of tokens and 10+ seconds per task taking screenshots at every tap.

**DroidPath is built from the ground up as an execution layer for autonomous AI agents:**

- **Zero-Roundtrip Multi-Hop Routing (Layer 3 A\*):** Streams live incremental accessibility diffs (`Shadow DOM`) from an on-device Kotlin daemon over loopback TCP, compiles screens into a persistent **Spatial Graph** (`shadow_graph.json`), and executes multi-screen workflows locally in Rust with **1 LLM call instead of 4–10**.
- **Plug-and-Play Agent Schemas (OpenAI · Anthropic · MCP):** Export ready-to-use JSON function-calling schemas via CLI (`droidpath schemas`), live HTTP (`GET /schemas/openai.json`, `/schemas/anthropic.json`, `/schemas/mcp.json`), or static files in [`schemas/`](./schemas/) — zero custom parsers required.
- **Opt-In Hybrid UI Tree + Vision Streaming:** Need visual context for icons or canvases? Pass `include_vision: true` to `android_get_ui_tree` to receive both the structured UI tree and a downscaled, heavily compressed JPEG as multimodal MCP content (`text` + `image`). Strictly **off by default** and **hard-blocked on `FLAG_SECURE` screens**.
- **Self-Correcting Agentic Feedback Loop:** When an element isn't found or a route deviates, DroidPath never returns a bare `"Error"`. It returns structured, **PII-redacted telemetry** (`screen_delta`, newly appeared nodes, ranked `closest_candidates`, and `suggested_next_actions`) so agents recover in 1 turn.
- **Headless Docker-Android Fleets:** Scale parallel autonomous agents inside headless Linux containers with ephemeral non-colliding daemon ports (`--daemon-port 0`), `ADB_SERVER_SOCKET` routing, `sys.boot_completed` synchronization, and graceful `SIGTERM` shutdown.
- **Safety & Privacy by Construction:** Mandatory Human-in-the-Loop (`requires_confirmation`) gating for hazardous actions (payments, transfers, deletes), automatic `FLAG_SECURE` / biometric pause, on-device password masking, and Luhn card / OTP PII redaction before hashing or persistence.

---

## Agentic Benchmark Summary

Measured live over USB ADB on an unlocked **Google Pixel 10a (`com.android.settings`, 54 interactive nodes)** using `node scripts/benchmark_agentic.mjs --mode live --json` against compiled `target/release/droidpath-mcp`:

**Live Pixel 10a Hardware Primitives & End-to-End Suite:**
- **Shadow DOM Read Latency:** **`0.32 ms`** steady-state (`41.89 ms` cold connect) vs **`2,423.9 ms`** `adb uiautomator dump` (**7,575× faster** · `10.7 KB` vs `29.4 KB` XML)
- **Live E2E Multi-Step Navigation (`Display`, `Battery`, `About phone`):** **`100%` success (3/3)** at **`603 ms` avg Cold-Start** (Seed Graph hydration) and **`561 ms` avg Warm A\***
- **Opt-In Hybrid Vision (`include_vision: true`, `540×1212` JPEG q45 + 54 nodes):** **`452.33 ms` · `38.6 KB` JPEG** vs **`658.1 ms` · `235.3 KB` raw PNG** from `adb screencap -p` (**6.1× smaller payload**)

**Multi-Step Architecture & Cost Comparison (`$3.00 / 1M` input tokens · Replay Model):**

| Architecture | Avg LLM Calls / Task | Avg Tokens / Task | Avg Task Time | Est. Cost / 1k Tasks | Live Success Rate |
|---|---:|---:|---:|---:|---:|
| **DroidPath (Shadow DOM + A\* Router)** | **1.0** | **39** *(warm A\*)* / **~1,135** *(cold tree)* | **464 ms** *(561 ms live)* | **$0.12** | **100% (3/3 live)** |
| **Pure Vision (Screenshots + VLM)** | 4.2 | 7,673 | 9,954 ms | $23.02 *(192× higher)* | — |
| **Appium / `uiautomator` XML** | 4.2 | 29,400 | 8,946 ms | $88.20 *(735× higher)* | — |

Full hardware & replay methodology and reproduction commands are in [**docs/benchmarks.md**](docs/benchmarks.md).

---

## Quick Start (1-Click Native Install)

**Prerequisites:** Android 8.0+ (API 26+) with USB/Wireless Debugging (no root, no companion APK) and `adb` (`brew install android-platform-tools` / `winget install Google.PlatformTools` / `sudo apt install adb`).

**macOS & Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.sh | bash -s -- --configure-mcp all --deploy
```

**Windows (PowerShell):**
```powershell
$env:DROIDPATH_DEPLOY="1"; $env:DROIDPATH_CONFIGURE_MCP="all"; irm https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.ps1 | iex
```

Verify installation and start the agent API:
```bash
droidpath-mcp doctor                        # Verify ADB, devices, daemon JAR & Spatial Graph
droidpath-mcp --stdio                       # Run MCP server over stdio (Claude, Cursor, Gemini CLI)
droidpath-mcp --http --port 8766            # Run loopback HTTP/SSE + /schemas/* API
droidpath-mcp schemas --format openai       # Print standardized OpenAI function-calling schema
```

---

## Agent Framework & SDK Integrations

### LangChain (`langchain-mcp-adapters`)
```python
from langchain_mcp_adapters.client import MultiServerMCPClient
from langgraph.prebuilt import create_react_agent

async with MultiServerMCPClient({
    "droidpath": {"command": "droidpath-mcp", "args": ["--stdio"], "transport": "stdio"}
}) as client:
    agent = create_react_agent("anthropic:claude-3-7-sonnet-latest", client.get_tools())
    await agent.ainvoke({"messages": [("user", "Open Display settings and enable Night Light.")]})
```

### CrewAI (`MCPServerAdapter`)
```python
from crewai import Agent, Task, Crew
from crewai_tools import MCPServerAdapter
from mcp import StdioServerParameters

with MCPServerAdapter(StdioServerParameters(command="droidpath-mcp", args=["--stdio"])) as tools:
    operator = Agent(role="Android Operator", goal="Execute Android workflows", backstory="...", tools=tools)
    task = Task(description="Navigate to Battery Saver via android_navigate_to_node(auto_explore=true).", expected_output="Arrival JSON", agent=operator)
    Crew(agents=[operator], tasks=[task]).kickoff()
```

### OpenAI & Anthropic SDKs (Zero-Parser Schemas)
```bash
# Export static JSON schemas or fetch live from http://127.0.0.1:8766/schemas/*
droidpath-mcp schemas --format openai    > openai-tools.json
droidpath-mcp schemas --format anthropic > anthropic-tools.json
```

See [**docs/integrations.md**](docs/integrations.md) for complete Python/TypeScript examples covering **OpenAI SDK**, **Anthropic SDK (with opt-in hybrid vision)**, **LangChain**, **CrewAI**, **MCP**, and the **Self-Correcting Feedback Loop**.

---

## Headless Docker-Android Agent Fleets

Run parallel autonomous Android agents without any GUI or VNC overhead:

```bash
export DROIDPATH_AUTH_TOKEN="local-dev-token"
docker compose -f docker-compose.headless.yml up --build -d
```

- Dynamic ephemeral ports (`--daemon-port 0`) prevent TCP collisions across parallel workers.
- `DROIDPATH_HEADLESS=1` waits for `sys.boot_completed=1` and enables exponential backoff `UiAutomation` retries.
- `ADB_SERVER_SOCKET` routes ADB cleanly across Docker networks or to a host ADB server.

See [**docs/headless-docker.md**](docs/headless-docker.md) for fleet scaling and macOS/CI host-ADB recipes.

---

## Standardized Agent Tool Surface (9 Tools)

All 9 tools are exposed consistently across MCP (`tools/list`), OpenAI (`schemas/openai-tools.json`), and Anthropic (`schemas/anthropic-tools.json`):

| Tool | Role |
|---|---|
| `android_navigate_to_node` | Primary multi-hop A\* graph router (`auto_explore: true` falls back to live discovery). |
| `android_explore_and_click` | Live Shadow DOM semantic search + bounded auto-scroll + graph edge recording. |
| `android_get_ui_tree` | Live compact UI tree inspection + opt-in hybrid JPEG vision (`include_vision: true`, blocked on `FLAG_SECURE`). |
| `android_graph_stats` | Read-only Spatial Graph telemetry (vertices, edges, active hash, daemon status). |
| `android_list_devices` | Enumerates connected USB, Wi-Fi, and emulator ADB targets. |
| `android_tap_element` | Direct pixel or selector tap with structured `feedback` telemetry on miss. |
| `android_swipe` | Zero-fling daemon touch swipe / drag with graph recording. |
| `android_type_text` | ASCII & Unicode text entry (safe shell quoting / clipboard paste) + optional ENTER. |
| `android_keyevent` | System & hardware keyevents (`HOME`, `BACK`, `ENTER`, `APP_SWITCH`, etc.). |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│  AI Agents (LangChain · CrewAI · Claude · Cursor · OpenAI · Anthropic)  │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │ MCP (stdio / HTTP+SSE) + /schemas/*.json
                                ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  droidpath-mcp & droidpath-core (Native Rust · Zero JVM · ~2 MB)        │
│  • Layer 3 A* Spatial Graph Router & Persistent shadow_graph.json       │
│  • Self-Correcting Agentic Feedback Loop (Delta + PII-Redacted Ranking) │
│  • Hazardous HITL Gatekeeper & FLAG_SECURE Privacy Pause                │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │ Loopback TCP NDJSON (:27189 or ephemeral :0)
                                ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  android-daemon.jar (Single-DEX Kotlin · app_process · No Root/APK)     │
│  • Event-Driven UiAutomation Shadow DOM Diffs (<1 ms incremental)       │
│  • Native Zero-Fling Input Injection & Opt-In Compressed Hybrid Vision  │
└─────────────────────────────────────────────────────────────────────────┘
```

DroidPath is **100% `shadow_graph.json` compatible** with native macOS [**AndOnMac**](https://github.com/V0val0x/AndOnMac) and includes an optional **Tauri v2 Desktop HUD** (`apps/droidpath-desktop`) for live visual debugging and graph inspection.

---

## Building & Testing from Source

```bash
# Rust core & MCP server
cargo test -p droidpath-core -p droidpath-mcp
cargo clippy -p droidpath-core -p droidpath-mcp --all-targets -- -D warnings

# Kotlin Android daemon (tests + single-DEX android-daemon.jar)
./android-daemon/scripts/build-dex.sh
node packages/droidpath-cli/scripts/sync-daemon.mjs

# Node CLI
node --test packages/droidpath-cli/test/cli.test.mjs

# Reproducible Agentic Benchmark
node scripts/benchmark_agentic.mjs
```

---

## Community & License

- [Contributing Guide](CONTRIBUTING.md) · [Code of Conduct](CODE_OF_CONDUCT.md) · [Security Policy](SECURITY.md) · [Accessibility](ACCESSIBILITY.md)
- Licensed under the [MIT License](LICENSE).
