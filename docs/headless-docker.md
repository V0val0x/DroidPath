# Headless Android Emulators & Docker Agent Fleets

DroidPath v2.3+ supports running autonomous Android AI agent fleets inside headless Linux containers (no X11, no VNC, no desktop HUD required).

## Key Headless Capabilities

1. **Dynamic Non-Colliding Daemon Ports (`--daemon-port 0`)**
   - Both `droidpath-mcp` and `droidpath` CLI accept `--daemon-port 0` (and `android-daemon` accepts `--port=0`), automatically binding an OS-assigned ephemeral loopback port so dozens of parallel emulator workers never collide on `:27189`.
2. **Boot Synchronization & `UiAutomation` Connect Retries**
   - Setting `DROIDPATH_HEADLESS=1` (or passing `--headless` to the CLI) waits for `sys.boot_completed=1` before deploying and launches `android-daemon` with `--wait-boot --connect-retries=12` (exponential backoff up to 3s per attempt) so cold headless emulators never fail on early `AccessibilityManagerService` races.
3. **Remote `ADB_SERVER_SOCKET` Propagation**
   - Both `droidpath-core` (Rust) and `droidpath-cli` (Node) honor `ADB_SERVER_SOCKET` (and `DROIDPATH_ADB_SERVER_SOCKET`), allowing containers to talk to a shared host or sidecar ADB server (e.g., `tcp:host.docker.internal:5037` or `tcp:android-emulator-1:5037`).
4. **Auth-Gated Container Bind (`--bind-host` / `DROIDPATH_BIND_HOST`)**
   - `droidpath-mcp --http` binds strict `127.0.0.1` loopback by default.
   - Inside Docker bridge networks (where host `127.0.0.1:8766:8766` forwards to the container bridge IP), explicitly pass `--bind-host 0.0.0.0` (or set `DROIDPATH_BIND_HOST=0.0.0.0`) together with `--auth-token <secret>` (or `DROIDPATH_AUTH_TOKEN=<secret>`). Non-loopback binds are refused unless a non-empty auth token is provided.
5. **Graceful `SIGTERM` Shutdown**
   - `droidpath-mcp --http` traps both `SIGINT` and `SIGTERM`, cleanly flushing `shadow_graph.json` and tearing down TCP streams before container exit.
   - `android-daemon` detaches cleanly via `UiAutomation.disconnect()` on shutdown hooks.

---

## Quick Start with Docker Compose

```bash
export DROIDPATH_AUTH_TOKEN="your-local-secret"
docker compose -f docker-compose.headless.yml up --build -d
```

Verify the headless agent server and inspect its exported OpenAI/Anthropic/MCP tool schemas:

```bash
curl -H "Authorization: Bearer $DROIDPATH_AUTH_TOKEN" http://127.0.0.1:8766/health
curl -H "Authorization: Bearer $DROIDPATH_AUTH_TOKEN" http://127.0.0.1:8766/schemas/openai.json
```

---

## Scaling Parallel Agent Fleets

To run $N$ parallel autonomous agents (e.g. for WebArena/AndroidWorld evaluation or parallel scraping/workflows):

1. Give each worker its own emulator serial (`--serial emulator-5554`, `emulator-5556`, …) and `--daemon-port 0`.
2. Mount a **shared read-only seed graph** or per-worker `shadow_graph.json` path (`--graph-path /data/worker-${ID}.json`) to avoid write contention.
3. On macOS / Windows hosts without `/dev/kvm` inside Docker Desktop, run the Android Emulator on the host and point the container at the host ADB server:

```bash
# On host:
adb -a -P 5037 nodaemon server &

# Inside container:
docker run --rm -e DROIDPATH_HEADLESS=1 \
  -e ADB_SERVER_SOCKET=tcp:host.docker.internal:5037 \
  -p 127.0.0.1:8766:8766 \
  droidpath-mcp --http --bind-host 0.0.0.0 --port 8766 --daemon-port 0 \
  --auth-token "${DROIDPATH_AUTH_TOKEN:-changeme-local-token}"
```
