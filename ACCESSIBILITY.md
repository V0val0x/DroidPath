# Accessibility Statement — DroidPath

**DroidPath** is committed to making AI-driven Android automation and our cross-platform Desktop HUD accessible to developers, QA engineers, and assistive-technology users.

## Accessibility Design & Commitments

- **Accessibility-Tree First Automation:** DroidPath reads and navigates Android applications using Android's `AccessibilityNodeInfo` / `UiAutomation` semantic tree (`text`, `contentDescription`, `resourceId`, bounds, and clickable/scrollable roles) rather than relying solely on raw pixel coordinates.
- **Machine-Readable CLI & MCP Interfaces:** `droidpath-mcp` exposes standard JSON-RPC 2.0 over `stdio` / loopback `HTTP/SSE`, and `packages/droidpath-cli` supports `--json` output across `doctor`, `init`, `rules`, `seed`, and `daemon` subcommands for clean consumption by screen readers and scripts.
- **Non-Color-Only Telemetry:** While the **Tauri v2 Desktop HUD** uses 3 status colors (**Red** autonomous execution, **Yellow** awaiting HITL confirmation, **Blue** awaiting biometrics/`FLAG_SECURE`), every waiting state is also paired with explicit textual status messages, interactive *Approve / Cancel* buttons, and structured MCP JSON status codes (`status: "requires_confirmation"`, `status: "secure_screen_active"`).
- **Seed Graph Clarity:** The Tauri v2 Spatial Graph Explorer renders clear semantic labels and distinct dashed vs solid geometry for unhydrated Seed nodes vs calibrated device nodes so state differences do not depend on color alone.

## Supported Environments

- **Desktop Hosts:** Windows 10/11 (x64/ARM64), macOS 14+ (Apple Silicon/Intel), and Linux (X11 and Wayland)
- **Android Targets:** Android 8.0+ (API 26+) with USB or Wireless ADB debugging enabled

## Known Limitations

- Third-party Android apps or games built with custom OpenGL/Vulkan/Flutter/Unity canvases that omit Android accessibility semantics may expose limited nodes in the Shadow DOM tree.
- Screens protected with Android `FLAG_SECURE` or system biometric dialogs intentionally suppress tree inspection for privacy (`SECURE_SCREEN_ACTIVE`), requiring physical user interaction on the device.

## Reporting Accessibility Barriers

If you encounter any accessibility barrier in the Tauri v2 Desktop HUD, CLI output, documentation, or MCP workflows, please open an issue using our [Bug Report template](https://github.com/V0val0x/DroidPath/issues/new/choose) and describe your assistive technology, OS, and reproduction steps.
