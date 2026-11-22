import { existsSync, mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export const RULES_START_MARKER = "<!-- DROIDPATH_MCP_RULES_START -->";
export const RULES_END_MARKER = "<!-- DROIDPATH_MCP_RULES_END -->";

export const SUPPORTED_CLIENTS = [
  "cursor",
  "claude",
  "claude-desktop",
  "claude-code",
  "gemini",
  "gemini-cli",
  "antigravity",
  "lm-studio",
  "lmstudio",
  "ollama",
  "openwebui",
  "local",
];

export function normalizeClientName(client) {
  switch (client) {
    case "claude-desktop":
      return "claude";
    case "gemini-cli":
      return "gemini";
    case "lmstudio":
      return "lm-studio";
    default:
      return client;
  }
}

export function isLocalAiClient(client) {
  const norm = normalizeClientName(client);
  return norm === "local" || norm === "ollama" || norm === "openwebui" || norm === "lm-studio";
}

export function resolveClientConfigPath(
  client,
  { platform = process.platform, env = process.env, homeDir = os.homedir() } = {}
) {
  const targetPath = platform === "win32" ? path.win32 : path.posix;
  switch (normalizeClientName(client)) {
    case "cursor":
      return targetPath.join(homeDir, ".cursor", "mcp.json");
    case "claude":
      if (platform === "darwin") {
        return targetPath.join(
          homeDir,
          "Library",
          "Application Support",
          "Claude",
          "claude_desktop_config.json"
        );
      }
      if (platform === "win32") {
        const appData = env.APPDATA || targetPath.join(homeDir, "AppData", "Roaming");
        return targetPath.join(appData, "Claude", "claude_desktop_config.json");
      }
      return targetPath.join(homeDir, ".config", "Claude", "claude_desktop_config.json");
    case "claude-code":
      return targetPath.join(homeDir, ".claude.json");
    case "gemini":
      return targetPath.join(homeDir, ".gemini", "settings.json");
    case "antigravity":
      return targetPath.join(homeDir, ".gemini", "antigravity", "mcp_config.json");
    case "lm-studio":
      return targetPath.join(homeDir, ".lmstudio", "mcp.json");
    case "ollama":
    case "openwebui":
    case "local": {
      const droidpathHome = env.DROIDPATH_HOME || targetPath.join(homeDir, ".droidpath");
      return targetPath.join(droidpathHome, "local-ai", "mcp-http.json");
    }
    default:
      throw new Error(
        `Unsupported MCP client '${client}'. Expected cursor, claude, claude-code, gemini, antigravity, lm-studio, ollama, openwebui, local, or all.`
      );
  }
}

export function defaultCliScriptPath() {
  const candidate = path.resolve(__dirname, "..", "bin", "droidpath.mjs");
  return existsSync(candidate) ? candidate : null;
}

export function buildMcpServerEntry({
  mode = "stdio",
  binaryPath = null,
  jarPath = null,
  port = 8766,
  daemonPort = 27189,
  cliScriptPath = defaultCliScriptPath(),
  nodePath = process.execPath,
} = {}) {
  if (mode === "http") {
    return {
      url: `http://127.0.0.1:${port}/sse`,
    };
  }
  if (binaryPath) {
    const args = ["--stdio"];
    if (daemonPort !== 27189) {
      args.push("--daemon-port", String(daemonPort));
    }
    if (jarPath) {
      args.push("--daemon-jar", jarPath);
    }
    return {
      command: binaryPath,
      args,
    };
  }
  if (cliScriptPath) {
    const args = [cliScriptPath, "mcp", "--stdio"];
    if (daemonPort !== 27189) {
      args.push("--daemon-port", String(daemonPort));
    }
    if (jarPath) {
      args.push("--jar", jarPath);
    }
    return {
      command: nodePath || "node",
      args,
    };
  }
  const args = ["--stdio"];
  if (daemonPort !== 27189) {
    args.push("--daemon-port", String(daemonPort));
  }
  if (jarPath) {
    args.push("--daemon-jar", jarPath);
  }
  return {
    command: "droidpath-mcp",
    args,
  };
}

function atomicWriteUtf8(targetPath, content) {
  const dir = path.dirname(targetPath);
  mkdirSync(dir, { recursive: true });
  const tmpPath = path.join(dir, `.${path.basename(targetPath)}.${process.pid}.${Date.now()}.tmp`);
  writeFileSync(tmpPath, content, "utf8");
  try {
    renameSync(tmpPath, targetPath);
  } catch (err) {
    try {
      unlinkSync(tmpPath);
    } catch {
      // Ignore cleanup error
    }
    throw err;
  }
}

export function configureMcpClient({
  client,
  configPath,
  entry,
  port = 8766,
  serverName = "droidpath",
  localAiDir = null,
  dryRun = false,
} = {}) {
  const normalizedClient = normalizeClientName(client);
  const targetPath = configPath || resolveClientConfigPath(normalizedClient);
  let doc = {};

  if (existsSync(targetPath)) {
    const raw = readFileSync(targetPath, "utf8").trim();
    if (raw.length > 0) {
      try {
        const parsed = JSON.parse(raw);
        if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
          throw new Error("Root JSON value is not an object.");
        }
        doc = parsed;
      } catch (err) {
        throw new Error(`Refusing to overwrite invalid JSON in ${targetPath}: ${err.message}`);
      }
    }
  }

  if (!doc.mcpServers || typeof doc.mcpServers !== "object" || Array.isArray(doc.mcpServers)) {
    doc.mcpServers = {};
  }

  const effectiveEntry =
    normalizedClient === "local" || normalizedClient === "ollama" || normalizedClient === "openwebui"
      ? {
          transport: "sse",
          url: `http://127.0.0.1:${port}/sse`,
          messageUrl: `http://127.0.0.1:${port}/message`,
          startCommand: `droidpath-mcp --http --port ${port}`,
        }
      : entry;

  const before = JSON.stringify(doc);
  doc.mcpServers[serverName] = effectiveEntry;
  if (normalizedClient === "local" || normalizedClient === "ollama" || normalizedClient === "openwebui") {
    const bundleDir = localAiDir || path.dirname(targetPath);
    doc.localAi = {
      loopbackOnly: true,
      sseUrl: `http://127.0.0.1:${port}/sse`,
      messageUrl: `http://127.0.0.1:${port}/message`,
      healthUrl: `http://127.0.0.1:${port}/health`,
      openAiToolsFile: path.join(bundleDir, "openai-tools.json"),
      systemPromptFile: path.join(bundleDir, "system-prompt.md"),
      compatibleClients: ["ollama", "lm-studio", "openwebui", "llama.cpp", "mlx", "on-device-openai"],
    };
  }
  const after = JSON.stringify(doc);
  const changed = before !== after || !existsSync(targetPath);

  if (!dryRun && changed) {
    atomicWriteUtf8(targetPath, `${JSON.stringify(doc, null, 2)}\n`);
  }

  return {
    client: normalizedClient,
    configPath: targetPath,
    serverName,
    changed,
    dryRun,
    entry: effectiveEntry,
  };
}

export function bundledRulesAssetPath() {
  const candidates = [
    path.resolve(__dirname, "..", "assets", "droidpath-agent-rules.md"),
    path.resolve(process.cwd(), "packages", "droidpath-cli", "assets", "droidpath-agent-rules.md"),
  ];
  return candidates.find((p) => existsSync(p)) || null;
}

export function loadCanonicalAgentRules() {
  const assetPath = bundledRulesAssetPath();
  if (assetPath) {
    return readFileSync(assetPath, "utf8").trim();
  }
  return buildCompactLocalSystemPrompt();
}

export function buildCompactLocalSystemPrompt({ port = 8766 } = {}) {
  return [
    `You control an Android device via DroidPath MCP tools over localhost (127.0.0.1:${port}).`,
    "Rules:",
    "1. Call `android_navigate_to_node(target, auto_explore: true)` first to reach screens in 1 step via the persistent A* Spatial Graph + automatic live fallback.",
    "2. Call `android_explore_and_click(target)` to discover, auto-scroll to, and click a live UI element while recording new edges.",
    "3. NEVER call `android_screenshot` while Shadow DOM is active; use `android_get_ui_tree` only if raw UI nodes are required.",
    "4. If a tool returns `status: \"needs_selection\"`, choose from the returned top-20 `candidates` instead of guessing.",
    "5. If a tool returns `status: \"requires_confirmation\"` (payments, purchases, deletions), STOP and ask the user in chat before retrying with `confirmed: true`.",
    "6. If a tool returns `status: \"secure_screen_active\"`, STOP and ask the user to unlock on the device. Never ask for or echo passwords, card PANs, or OTP codes.",
  ].join("\n");
}

export function buildOpenAIToolSchemas() {
  const serialProp = {
    type: "string",
    description: "Optional ADB device serial. Defaults to the first connected device.",
  };

  const rawTools = [
    {
      name: "android_navigate_to_node",
      description:
        "Layer 3 Persistent A* Symbolic UI Router. Computes the shortest path across known screens in the Spatial Graph and executes taps/swipes via live Shadow DOM feedback. Set auto_explore: true to fall back to live discovery automatically.",
      parameters: {
        type: "object",
        properties: {
          target: {
            type: "string",
            description: "Target UI text, resource-id, package name, or screen title.",
          },
          target_hash: {
            type: "string",
            description: "Optional exact target layout hash in 0x... hex or decimal format.",
          },
          max_steps: { type: "integer", description: "Max navigation hops (1-30). Default: 12." },
          step_timeout_ms: { type: "integer", description: "Per-step settle timeout in ms. Default: 2000." },
          confirmed: {
            type: "boolean",
            description: "Set true ONLY after explicit human confirmation for hazardous actions. Default: false.",
          },
          auto_explore: {
            type: "boolean",
            description: "Automatically fall back to live explore-and-click if no graph route exists. Default: false.",
          },
          device_serial: serialProp,
        },
        required: [],
      },
    },
    {
      name: "android_explore_and_click",
      description:
        "Dynamic Shadow DOM discovery + click with optional A* prefix routing, bounded scrolling (down/up), hazardous HITL guard, and DAG edge recording. Returns top-20 PII-scrubbed candidates (needs_selection) if not found.",
      parameters: {
        type: "object",
        properties: {
          target: {
            type: "string",
            description: "Target label, resource_id suffix, content_desc, or breadcrumb (e.g. 'Display > Night Light').",
          },
          max_scrolls: { type: "integer", description: "Max discovery scrolls (0-10). Default: 3." },
          step_timeout_ms: { type: "integer", description: "Per-step settle timeout in ms. Default: 2000." },
          confirmed: {
            type: "boolean",
            description: "Set true ONLY after explicit human confirmation for hazardous actions. Default: false.",
          },
          prefer_graph_route: {
            type: "boolean",
            description: "Try known A* route / prefix vertex first before live scan. Default: true.",
          },
          device_serial: serialProp,
        },
        required: ["target"],
      },
    },
    {
      name: "android_get_ui_tree",
      description: "Reads interactive Android UI elements from the sub-millisecond Shadow DOM stream.",
      parameters: {
        type: "object",
        properties: { device_serial: serialProp },
        required: [],
      },
    },
    {
      name: "android_tap_element",
      description: "Taps an Android UI element by (x, y) or semantic text / resource_id / content_desc.",
      parameters: {
        type: "object",
        properties: {
          x: { type: "integer" },
          y: { type: "integer" },
          text: { type: "string" },
          resource_id: { type: "string" },
          content_desc: { type: "string" },
          device_serial: serialProp,
        },
        required: [],
      },
    },
    {
      name: "android_swipe",
      description: "Performs a touch swipe from (x1, y1) to (x2, y2).",
      parameters: {
        type: "object",
        properties: {
          x1: { type: "integer" },
          y1: { type: "integer" },
          x2: { type: "integer" },
          y2: { type: "integer" },
          duration_ms: { type: "integer" },
          device_serial: serialProp,
        },
        required: ["x1", "y1", "x2", "y2"],
      },
    },
    {
      name: "android_type_text",
      description: "Types ASCII or Unicode text into the focused Android input field.",
      parameters: {
        type: "object",
        properties: {
          text: { type: "string" },
          press_enter: { type: "boolean" },
          device_serial: serialProp,
        },
        required: ["text"],
      },
    },
    {
      name: "android_keyevent",
      description: "Sends an Android keyevent (HOME, BACK, ENTER, APP_SWITCH, DELETE).",
      parameters: {
        type: "object",
        properties: {
          keycode: { type: "string" },
          device_serial: serialProp,
        },
        required: ["keycode"],
      },
    },
    {
      name: "android_graph_stats",
      description: "Returns Spatial Graph telemetry (vertices, edges, active hash, daemon state).",
      parameters: { type: "object", properties: {}, required: [] },
    },
    {
      name: "android_list_devices",
      description: "Lists connected Android devices visible over USB or Wireless ADB.",
      parameters: { type: "object", properties: {}, required: [] },
    },
  ];

  return rawTools.map((t) => ({
    type: "function",
    function: {
      name: t.name,
      description: t.description,
      parameters: t.parameters,
    },
  }));
}

export function resolveClientRulesPaths(
  client,
  {
    projectDir = process.cwd(),
    homeDir = os.homedir(),
    globalInstall = false,
    platform = process.platform,
  } = {}
) {
  const p = platform === "win32" ? path.win32 : path.posix;
  const norm = normalizeClientName(client);
  switch (norm) {
    case "cursor":
      return [p.join(projectDir, ".cursorrules")];
    case "claude":
    case "claude-code": {
      const out = [p.join(projectDir, "CLAUDE.md")];
      if (globalInstall) {
        out.push(p.join(homeDir, ".claude", "CLAUDE.md"));
      }
      return out;
    }
    case "gemini": {
      const out = [p.join(projectDir, "GEMINI.md")];
      if (globalInstall) {
        out.push(p.join(homeDir, ".gemini", "GEMINI.md"));
      }
      return out;
    }
    case "antigravity": {
      const out = [
        p.join(projectDir, ".agent", "rules", "droidpath.md"),
        p.join(projectDir, "GEMINI.md"),
      ];
      if (globalInstall) {
        out.push(p.join(homeDir, ".gemini", "GEMINI.md"));
      }
      return out;
    }
    case "lm-studio":
    case "ollama":
    case "openwebui":
    case "local":
      return [];
    default:
      throw new Error(`Unsupported rules client '${client}'.`);
  }
}

export function mergeManagedRulesBlock(existingText, bodyContent) {
  const managedBlock = `${RULES_START_MARKER}\n${bodyContent.trim()}\n${RULES_END_MARKER}`;
  if (!existingText || existingText.length === 0) {
    return {
      text: `${managedBlock}\n`,
      action: "created",
    };
  }

  const startIdx = existingText.indexOf(RULES_START_MARKER);
  const endIdx = existingText.indexOf(RULES_END_MARKER);

  if (startIdx === -1 && endIdx === -1) {
    const sep = existingText.endsWith("\n\n")
      ? ""
      : existingText.endsWith("\n")
        ? "\n"
        : "\n\n";
    return {
      text: `${existingText}${sep}${managedBlock}\n`,
      action: "appended",
    };
  }

  if (startIdx === -1 || endIdx === -1 || endIdx < startIdx) {
    throw new Error(
      `Malformed DroidPath rules markers (${RULES_START_MARKER} / ${RULES_END_MARKER}). Refusing to overwrite file.`
    );
  }

  const before = existingText.slice(0, startIdx);
  const after = existingText.slice(endIdx + RULES_END_MARKER.length);
  const updated = `${before}${managedBlock}${after}`;
  const nextText = updated.endsWith("\n") ? updated : `${updated}\n`;
  return {
    text: nextText,
    action: nextText === existingText ? "unchanged" : "updated",
  };
}

export function installAgentRulesFile({
  filePath,
  content = loadCanonicalAgentRules(),
  dryRun = false,
} = {}) {
  const existed = existsSync(filePath);
  const existingText = existed ? readFileSync(filePath, "utf8") : "";
  const { text: nextText, action } = mergeManagedRulesBlock(existingText, content);
  const changed = !existed || nextText !== existingText;

  if (!dryRun && changed) {
    atomicWriteUtf8(filePath, nextText);
  }

  return {
    filePath,
    changed,
    action: changed ? action : "unchanged",
    dryRun,
  };
}

export function installLocalAiBundle({
  outDir = path.join(process.env.DROIDPATH_HOME || path.join(os.homedir(), ".droidpath"), "local-ai"),
  port = 8766,
  dryRun = false,
} = {}) {
  const promptPath = path.join(outDir, "system-prompt.md");
  const toolsPath = path.join(outDir, "openai-tools.json");

  const promptContent = `${buildCompactLocalSystemPrompt({ port })}\n`;
  const toolsContent = `${JSON.stringify(
    {
      schemaVersion: 1,
      server: {
        name: "droidpath-mcp",
        loopbackOnly: true,
        sseUrl: `http://127.0.0.1:${port}/sse`,
        messageUrl: `http://127.0.0.1:${port}/message`,
        startCommand: `droidpath-mcp --http --port ${port}`,
      },
      tools: buildOpenAIToolSchemas(),
    },
    null,
    2
  )}\n`;

  const results = [];
  for (const [filePath, desired] of [
    [promptPath, promptContent],
    [toolsPath, toolsContent],
  ]) {
    const existed = existsSync(filePath);
    const prev = existed ? readFileSync(filePath, "utf8") : null;
    const changed = !existed || prev !== desired;
    if (!dryRun && changed) {
      atomicWriteUtf8(filePath, desired);
    }
    results.push({
      filePath,
      changed,
      action: !existed ? "created" : changed ? "updated" : "unchanged",
      dryRun,
    });
  }

  return results;
}
