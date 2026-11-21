import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import {
  bundledDaemonJarPath,
  checkTcpPort,
  deployDaemon,
  listDevices,
  resolveAdbPath,
  stopDaemon,
} from "./adb.mjs";
import {
  downloadNativeBinary,
  resolveDesktopHudBinaryPath,
  resolveMcpBinaryPath,
} from "./binary-resolver.mjs";
import {
  buildCompactLocalSystemPrompt,
  buildMcpServerEntry,
  buildOpenAIToolSchemas,
  configureMcpClient,
  installAgentRulesFile,
  installLocalAiBundle,
  isLocalAiClient,
  loadCanonicalAgentRules,
  normalizeClientName,
  resolveClientConfigPath,
  resolveClientRulesPaths,
} from "./mcp-config.mjs";

export const CLI_VERSION = "2.1.0";

export function usageText() {
  return `droidpath v${CLI_VERSION} — Zero-Friction 1-Click Android Shadow DOM & MCP CLI

USAGE:
  npx droidpath [COMMAND] [OPTIONS]

COMMANDS:
  start | up            1-click onboarding: auto-inject android-daemon.jar (adb push + forward + app_process)
                        and optionally configure MCP clients & agent rules
  init                  Auto-configure MCP configs + idempotent agent rules (Cursor, Claude, Gemini CLI,
                        Antigravity, LM Studio, Ollama/OpenWebUI/Local AI)
  rules                 Install/update idempotent agent rules (.cursorrules, CLAUDE.md, GEMINI.md,
                        .agent/rules/droidpath.md, local OpenAI tool schemas & compact system prompt)
  daemon <action>       Manage Android Shadow DOM daemon: deploy | start | stop | status
  mcp                   Launch native Rust droidpath-mcp server (--stdio or --http) with auto-daemon inject
  hud                   Launch cross-platform Tauri Desktop HUD
  doctor                Run full diagnostic check (ADB, devices, daemon JAR, :27189, MCP binary, configs)

OPTIONS:
  --serial <id>         Target ADB device serial (auto-selects first authorized device)
  --daemon-port <port>  Shadow DOM daemon TCP forward port (default: 27189)
  --jar <path>          Custom path to android-daemon.jar (defaults to bundled JAR)
  --client <name>       Target client(s): cursor | claude | claude-code | gemini | antigravity |
                        lm-studio | ollama | openwebui | local | all (comma-separated, default: all)
  --no-rules            Skip installing agent rule files during 'init'
  --dir <path>          Workspace directory for project rule files (default: cwd)
  --global              Also install global user-level rules (~/.gemini/GEMINI.md, ~/.claude/CLAUDE.md)
  --format <fmt>        For 'rules': print to stdout instead of writing (rules | prompt | local | openai)
  --http                Use HTTP/SSE transport instead of stdio
  --stdio               Use stdio JSON-RPC transport (default for mcp)
  --port <port>         HTTP/SSE listen port (default: 8766)
  --configure           Also configure MCP clients during 'start'
  --deploy              Also deploy android-daemon during 'init'
  --no-auto-deploy      Skip automatic daemon deploy before starting 'mcp'
  --dry-run             Preview config/rule changes without writing files
  --json                Machine-readable JSON output
  -v, --version         Print version
  -h, --help            Show help
`;
}

export function parseCliArgs(argv) {
  const flags = {
    serial: null,
    daemonPort: 27189,
    port: 8766,
    jar: null,
    client: "all",
    noRules: false,
    dir: null,
    global: false,
    format: null,
    http: false,
    stdio: false,
    configure: false,
    deploy: false,
    noAutoDeploy: false,
    dryRun: false,
    json: false,
    help: false,
    version: false,
  };
  const positional = [];

  const requireFlagValue = (flagName, idx) => {
    const val = argv[idx];
    if (val === undefined || val.startsWith("-")) {
      throw new Error(`Missing value for ${flagName}.`);
    }
    return val;
  };

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case "-h":
      case "--help":
        flags.help = true;
        break;
      case "-v":
      case "--version":
        flags.version = true;
        break;
      case "--serial":
        flags.serial = requireFlagValue("--serial", ++i);
        break;
      case "--daemon-port": {
        const parsed = Number.parseInt(requireFlagValue("--daemon-port", ++i), 10);
        if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
          throw new Error("Invalid value for --daemon-port (expected 1-65535).");
        }
        flags.daemonPort = parsed;
        break;
      }
      case "--port": {
        const parsed = Number.parseInt(requireFlagValue("--port", ++i), 10);
        if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
          throw new Error("Invalid value for --port (expected 1-65535).");
        }
        flags.port = parsed;
        break;
      }
      case "--jar":
      case "--daemon-jar":
        flags.jar = requireFlagValue(a, ++i);
        break;
      case "--client":
        flags.client = requireFlagValue("--client", ++i);
        break;
      case "--no-rules":
        flags.noRules = true;
        break;
      case "--dir":
        flags.dir = requireFlagValue("--dir", ++i);
        break;
      case "--global":
        flags.global = true;
        break;
      case "--format":
        flags.format = requireFlagValue("--format", ++i);
        break;
      case "--http":
        flags.http = true;
        break;
      case "--stdio":
        flags.stdio = true;
        break;
      case "--configure":
        flags.configure = true;
        break;
      case "--deploy":
        flags.deploy = true;
        break;
      case "--no-auto-deploy":
        flags.noAutoDeploy = true;
        break;
      case "--dry-run":
        flags.dryRun = true;
        break;
      case "--json":
        flags.json = true;
        break;
      default:
        positional.push(a);
        break;
    }
  }

  return { positional, flags };
}

export function expandClients(clientArg) {
  if (clientArg === "all") {
    return ["cursor", "claude", "claude-code", "gemini", "antigravity", "lm-studio", "local"];
  }
  const seen = new Set();
  const out = [];
  for (const raw of clientArg.split(",").map((s) => s.trim()).filter(Boolean)) {
    const norm = normalizeClientName(raw);
    if (!seen.has(norm)) {
      seen.add(norm);
      out.push(norm);
    }
  }
  return out;
}

function installRulesForClients(clients, { projectDir, homeDir, globalInstall, port, dryRun, localAiDir }) {
  const rulesContent = loadCanonicalAgentRules();
  const seenFiles = new Set();
  const results = [];
  let needsLocalBundle = false;

  for (const client of clients) {
    if (isLocalAiClient(client)) {
      needsLocalBundle = true;
    }
    const targetFiles = resolveClientRulesPaths(client, {
      projectDir,
      homeDir,
      globalInstall,
    });
    for (const filePath of targetFiles) {
      if (seenFiles.has(filePath)) continue;
      seenFiles.add(filePath);
      results.push(installAgentRulesFile({ filePath, content: rulesContent, dryRun }));
    }
  }

  if (needsLocalBundle) {
    const bundleDir = localAiDir || path.join(homeDir, ".droidpath", "local-ai");
    for (const r of installLocalAiBundle({ outDir: bundleDir, port, dryRun })) {
      if (!seenFiles.has(r.filePath)) {
        seenFiles.add(r.filePath);
        results.push(r);
      }
    }
  }

  return results;
}

export async function runCli(argv, io = { out: console.log, err: console.error }) {
  let positional;
  let flags;
  try {
    ({ positional, flags } = parseCliArgs(argv));
  } catch (err) {
    io.err(`[DroidPath Error] ${err.message}`);
    return 2;
  }

  if (flags.version) {
    io.out(`droidpath ${CLI_VERSION}`);
    return 0;
  }
  if (flags.help) {
    io.out(usageText());
    return 0;
  }

  const command = positional[0] || "start";
  const sub = positional[1] || null;
  const jarPath = flags.jar || bundledDaemonJarPath();
  const projectDir = flags.dir ? path.resolve(flags.dir) : process.cwd();
  const homeDir = os.homedir();

  try {
    if (command === "start" || command === "up") {
      const report = await deployDaemon({
        serial: flags.serial || undefined,
        port: flags.daemonPort,
        jarPath,
        logger: flags.json ? () => {} : (m) => io.err(`[DroidPath] ${m}`),
      });

      const mcpChanges = [];
      let ruleChanges = [];
      if (flags.configure) {
        const binaryPath = resolveMcpBinaryPath();
        const entry = buildMcpServerEntry({
          mode: flags.http ? "http" : "stdio",
          binaryPath,
          jarPath,
          port: flags.port,
          daemonPort: flags.daemonPort,
        });
        const clients = expandClients(flags.client);
        for (const client of clients) {
          mcpChanges.push(
            configureMcpClient({ client, entry, port: flags.port, dryRun: flags.dryRun })
          );
        }
        if (!flags.noRules) {
          ruleChanges = installRulesForClients(clients, {
            projectDir,
            homeDir,
            globalInstall: flags.global,
            port: flags.port,
            dryRun: flags.dryRun,
          });
        }
      }

      if (flags.json) {
        io.out(JSON.stringify({ daemon: report, mcp: mcpChanges, rules: ruleChanges }, null, 2));
      } else {
        io.out(`✅ DroidPath daemon ready on ${report.serial} (127.0.0.1:${report.port})`);
        if (report.pushed) {
          io.out(`   Deployed bundled android-daemon.jar (SHA-256 ${report.sha256?.slice(0, 12)}...)`);
        }
        for (const c of mcpChanges) {
          io.out(`✅ Configured ${c.client} MCP -> ${c.configPath}`);
        }
        for (const r of ruleChanges) {
          io.out(`✅ Rules (${r.action}) -> ${r.filePath}`);
        }
        if (!flags.configure) {
          io.out(`   Next: run \`npx droidpath init\` to auto-configure Cursor, Claude, Gemini, Antigravity & Local AI.`);
        }
      }
      return 0;
    }

    if (command === "init") {
      const binaryPath = resolveMcpBinaryPath();
      const entry = buildMcpServerEntry({
        mode: flags.http ? "http" : "stdio",
        binaryPath,
        jarPath,
        port: flags.port,
        daemonPort: flags.daemonPort,
      });

      const clients = expandClients(flags.client);
      const results = [];
      for (const client of clients) {
        results.push(
          configureMcpClient({ client, entry, port: flags.port, dryRun: flags.dryRun })
        );
      }

      const ruleResults = flags.noRules
        ? []
        : installRulesForClients(clients, {
            projectDir,
            homeDir,
            globalInstall: flags.global,
            port: flags.port,
            dryRun: flags.dryRun,
            localAiDir: flags.dir ? path.join(projectDir, ".droidpath-local-ai") : undefined,
          });

      let deployResult = null;
      if (flags.deploy) {
        deployResult = await deployDaemon({
          serial: flags.serial || undefined,
          port: flags.daemonPort,
          jarPath,
          logger: flags.json ? () => {} : (m) => io.err(`[DroidPath] ${m}`),
        });
      }

      if (flags.json) {
        io.out(
          JSON.stringify(
            { configured: results, rules: ruleResults, daemon: deployResult },
            null,
            2
          )
        );
      } else {
        for (const r of results) {
          const verb = r.dryRun
            ? "[dry-run] Would configure"
            : r.changed
              ? "✅ Configured"
              : "✓ Already configured";
          io.out(`${verb} ${r.client}: ${r.configPath}`);
        }
        for (const rr of ruleResults) {
          const verb = rr.dryRun
            ? `[dry-run] Would ${rr.action}`
            : rr.changed
              ? `✅ Rules (${rr.action})`
              : "✓ Rules up to date";
          io.out(`${verb}: ${rr.filePath}`);
        }
        if (deployResult) {
          io.out(`✅ Android daemon live on ${deployResult.serial} (127.0.0.1:${deployResult.port})`);
        }
      }
      return 0;
    }

    if (command === "rules") {
      if (flags.format) {
        const fmt = flags.format.toLowerCase();
        if (fmt === "rules" || fmt === "markdown" || fmt === "md") {
          io.out(loadCanonicalAgentRules());
          return 0;
        }
        if (fmt === "prompt" || fmt === "local-prompt" || fmt === "local" || fmt === "system-prompt") {
          io.out(buildCompactLocalSystemPrompt({ port: flags.port }));
          return 0;
        }
        if (fmt === "openai" || fmt === "openai-tools") {
          io.out(JSON.stringify(buildOpenAIToolSchemas(), null, 2));
          return 0;
        }
        io.err(`Unsupported --format '${flags.format}'. Expected: rules | prompt | local | openai`);
        return 2;
      }

      const clients = expandClients(flags.client);
      const ruleResults = installRulesForClients(clients, {
        projectDir,
        homeDir,
        globalInstall: flags.global,
        port: flags.port,
        dryRun: flags.dryRun,
        localAiDir: flags.dir ? path.join(projectDir, ".droidpath-local-ai") : undefined,
      });

      if (flags.json) {
        io.out(JSON.stringify({ rules: ruleResults }, null, 2));
      } else {
        for (const rr of ruleResults) {
          const verb = rr.dryRun
            ? `[dry-run] Would ${rr.action}`
            : rr.changed
              ? `✅ Rules (${rr.action})`
              : "✓ Rules up to date";
          io.out(`${verb}: ${rr.filePath}`);
        }
      }
      return 0;
    }

    if (command === "daemon") {
      const action = sub || "deploy";
      if (action === "deploy" || action === "start") {
        const res = await deployDaemon({
          serial: flags.serial || undefined,
          port: flags.daemonPort,
          jarPath,
          logger: flags.json ? () => {} : (m) => io.err(`[DroidPath] ${m}`),
        });
        io.out(
          flags.json
            ? JSON.stringify(res, null, 2)
            : `✅ Daemon running on ${res.serial} -> 127.0.0.1:${res.port}`
        );
        return 0;
      }
      if (action === "stop") {
        const res = stopDaemon({
          serial: flags.serial || undefined,
          port: flags.daemonPort,
        });
        io.out(
          flags.json
            ? JSON.stringify(res, null, 2)
            : `🛑 Stopped daemon on ${res.serial} and removed tcp:${res.port} forward.`
        );
        return 0;
      }
      if (action === "status") {
        const reachable = await checkTcpPort("127.0.0.1", flags.daemonPort, 400);
        const devices = (() => {
          try {
            return listDevices();
          } catch {
            return [];
          }
        })();
        const payload = { port: flags.daemonPort, reachable, devices };
        io.out(
          flags.json
            ? JSON.stringify(payload, null, 2)
            : `Daemon 127.0.0.1:${flags.daemonPort}: ${reachable ? "REACHABLE" : "OFFLINE"} (${devices.length} ADB device(s))`
        );
        return reachable ? 0 : 1;
      }
      io.err(`Unknown daemon action '${action}'. Use: deploy | start | stop | status`);
      return 2;
    }

    if (command === "mcp") {
      if (!flags.noAutoDeploy) {
        const alreadyUp = await checkTcpPort("127.0.0.1", flags.daemonPort, 250);
        if (!alreadyUp) {
          try {
            const devices = listDevices();
            if (devices.length > 0) {
              await deployDaemon({
                serial: flags.serial || devices[0].serial,
                port: flags.daemonPort,
                jarPath,
                logger: (m) => io.err(`[DroidPath] ${m}`),
              });
            }
          } catch (err) {
            io.err(`[DroidPath] Auto-deploy skipped: ${err.message}`);
          }
        }
      }

      let binPath = resolveMcpBinaryPath();
      if (!binPath) {
        binPath = await downloadNativeBinary({
          logger: (m) => io.err(`[DroidPath] ${m}`),
        });
      }

      const childArgs = [flags.http ? "--http" : "--stdio"];
      if (flags.http) childArgs.push("--port", String(flags.port));
      childArgs.push("--daemon-port", String(flags.daemonPort));
      if (jarPath) childArgs.push("--daemon-jar", jarPath);

      return await new Promise((resolve) => {
        const child = spawn(binPath, childArgs, { stdio: "inherit" });
        child.on("exit", (code) => resolve(code ?? 0));
        child.on("error", (err) => {
          io.err(`Failed to launch ${binPath}: ${err.message}`);
          resolve(1);
        });
      });
    }

    if (command === "hud") {
      const hudBin = resolveDesktopHudBinaryPath();
      if (!hudBin) {
        io.err(
          "DroidPath Desktop HUD binary not found. Build it with:\n  cd apps/droidpath-desktop && npm install && npm run tauri build"
        );
        return 1;
      }
      const child = spawn(hudBin, [], { detached: true, stdio: "ignore" });
      child.unref();
      io.out(`🚀 Launched DroidPath Desktop HUD (${hudBin})`);
      return 0;
    }

    if (command === "doctor") {
      const adbPath = resolveAdbPath();
      let devices = [];
      if (adbPath) {
        try {
          devices = listDevices({ adbPath });
        } catch {
          devices = [];
        }
      }
      const tcpReachable = await checkTcpPort("127.0.0.1", flags.daemonPort, 350);
      const mcpBin = resolveMcpBinaryPath();
      const hudBin = resolveDesktopHudBinaryPath();
      const graphPath = path.join(homeDir, ".droidpath", "shadow_graph.json");
      const cursorPath = resolveClientConfigPath("cursor");
      const claudePath = resolveClientConfigPath("claude");
      const claudeCodePath = resolveClientConfigPath("claude-code");
      const geminiPath = resolveClientConfigPath("gemini");
      const antigravityPath = resolveClientConfigPath("antigravity");
      const lmStudioPath = resolveClientConfigPath("lm-studio");
      const localAiPath = resolveClientConfigPath("local");

      const report = {
        cliVersion: CLI_VERSION,
        platform: `${process.platform}-${process.arch}`,
        nodeVersion: process.version,
        adb: { found: Boolean(adbPath), path: adbPath },
        devices,
        daemonJar: { found: Boolean(jarPath), path: jarPath },
        daemonTcp: { port: flags.daemonPort, reachable: tcpReachable },
        mcpBinary: { found: Boolean(mcpBin), path: mcpBin },
        desktopHud: { found: Boolean(hudBin), path: hudBin },
        spatialGraph: { path: graphPath, exists: existsSync(graphPath) },
        mcpConfigs: {
          cursor: { path: cursorPath, exists: existsSync(cursorPath) },
          claudeDesktop: { path: claudePath, exists: existsSync(claudePath) },
          claudeCode: { path: claudeCodePath, exists: existsSync(claudeCodePath) },
          gemini: { path: geminiPath, exists: existsSync(geminiPath) },
          antigravity: { path: antigravityPath, exists: existsSync(antigravityPath) },
          lmStudio: { path: lmStudioPath, exists: existsSync(lmStudioPath) },
          localAi: { path: localAiPath, exists: existsSync(localAiPath) },
        },
      };

      if (flags.json) {
        io.out(JSON.stringify(report, null, 2));
      } else {
        io.out(`DroidPath Doctor v${CLI_VERSION} (${report.platform})`);
        io.out(`  ADB:           ${report.adb.found ? `✅ ${report.adb.path}` : "❌ Not found"}`);
        io.out(`  Devices:       ${devices.length > 0 ? `✅ ${devices.map((d) => `${d.model} (${d.serial})`).join(", ")}` : "⚠️ None attached"}`);
        io.out(`  Daemon JAR:    ${report.daemonJar.found ? `✅ ${report.daemonJar.path}` : "❌ Missing"}`);
        io.out(`  Daemon :${flags.daemonPort}:  ${tcpReachable ? "✅ Reachable" : "⚠️ Offline (run `npx droidpath start`)"}`);
        io.out(`  MCP Binary:    ${report.mcpBinary.found ? `✅ ${report.mcpBinary.path}` : "⚠️ Not installed (auto-downloads on first `mcp` run)"}`);
        io.out(`  Desktop HUD:   ${report.desktopHud.found ? `✅ ${report.desktopHud.path}` : "ℹ️ Optional (Tauri v2 app)"}`);
        io.out(`  Spatial Graph: ${report.spatialGraph.exists ? `✅ ${graphPath}` : `ℹ️ Will be created at ${graphPath}`}`);
      }
      return 0;
    }

    io.err(`Unknown command '${command}'. Run 'droidpath --help' for usage.`);
    return 2;
  } catch (err) {
    io.err(`[DroidPath Error] ${err.message}`);
    return 1;
  }
}
