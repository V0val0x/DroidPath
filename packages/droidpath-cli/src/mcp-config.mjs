import { existsSync, mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

export function resolveClientConfigPath(client, { platform = process.platform, env = process.env, homeDir = os.homedir() } = {}) {
  const targetPath = platform === "win32" ? path.win32 : path.posix;
  switch (client) {
    case "cursor":
      return targetPath.join(homeDir, ".cursor", "mcp.json");
    case "claude":
    case "claude-desktop":
      if (platform === "darwin") {
        return targetPath.join(homeDir, "Library", "Application Support", "Claude", "claude_desktop_config.json");
      }
      if (platform === "win32") {
        const appData = env.APPDATA || targetPath.join(homeDir, "AppData", "Roaming");
        return targetPath.join(appData, "Claude", "claude_desktop_config.json");
      }
      return targetPath.join(homeDir, ".config", "Claude", "claude_desktop_config.json");
    case "claude-code":
      return targetPath.join(homeDir, ".claude.json");
    default:
      throw new Error(`Unsupported MCP client '${client}'. Expected cursor, claude, claude-code, or all.`);
  }
}

export function buildMcpServerEntry({
  mode = "stdio",
  binaryPath = null,
  jarPath = null,
  port = 8766,
  daemonPort = 27189,
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
  const args = ["-y", "droidpath", "mcp", "--stdio"];
  if (daemonPort !== 27189) {
    args.push("--daemon-port", String(daemonPort));
  }
  return {
    command: "npx",
    args,
  };
}

export function configureMcpClient({
  client,
  configPath,
  entry,
  serverName = "droidpath",
  dryRun = false,
} = {}) {
  const targetPath = configPath || resolveClientConfigPath(client);
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

  const before = JSON.stringify(doc.mcpServers[serverName] ?? null);
  doc.mcpServers[serverName] = entry;
  const after = JSON.stringify(doc.mcpServers[serverName]);
  const changed = before !== after || !existsSync(targetPath);

  if (!dryRun && changed) {
    const dir = path.dirname(targetPath);
    mkdirSync(dir, { recursive: true });
    const tmpPath = path.join(dir, `.${path.basename(targetPath)}.${process.pid}.${Date.now()}.tmp`);
    writeFileSync(tmpPath, `${JSON.stringify(doc, null, 2)}\n`, "utf8");
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

  return {
    client,
    configPath: targetPath,
    serverName,
    changed,
    dryRun,
    entry,
  };
}
