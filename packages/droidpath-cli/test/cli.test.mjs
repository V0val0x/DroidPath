import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  bundledDaemonJarPath,
  checkTcpPort,
  deployDaemon,
  parseDevicesOutput,
  sha256File,
  validateSerial,
} from "../src/adb.mjs";
import { releaseAssetName } from "../src/binary-resolver.mjs";
import { parseCliArgs, runCli, expandClients } from "../src/index.mjs";
import {
  RULES_END_MARKER,
  RULES_START_MARKER,
  buildCompactLocalSystemPrompt,
  buildMcpServerEntry,
  buildOpenAIToolSchemas,
  configureMcpClient,
  installAgentRulesFile,
  installLocalAiBundle,
  loadCanonicalAgentRules,
  resolveClientConfigPath,
  resolveClientRulesPaths,
} from "../src/mcp-config.mjs";

test("validateSerial rejects command injection and flags", () => {
  assert.equal(validateSerial("61061JEA307110"), true);
  assert.equal(validateSerial("192.168.1.42:5555"), true);
  assert.equal(validateSerial("adb-6106-abc._tls-connect"), true);

  assert.equal(validateSerial(""), false);
  assert.equal(validateSerial("-s"), false);
  assert.equal(validateSerial("6106; rm -rf /"), false);
  assert.equal(validateSerial("serial with spaces"), false);
});

test("parseDevicesOutput extracts USB and Wi-Fi devices while skipping offline", () => {
  const sample = `
* daemon started successfully
List of devices attached
61061JEA307110         device usb:1-1 product:stallion model:Pixel_10a device:stallion transport_id:4
192.168.1.80:5555      device product:panther model:Pixel_7 device:panther transport_id:5
offline_serial         offline transport_id:6
`;
  const devices = parseDevicesOutput(sample);
  assert.equal(devices.length, 2);
  assert.deepEqual(devices[0], {
    serial: "61061JEA307110",
    model: "Pixel 10a",
    isWireless: false,
  });
  assert.deepEqual(devices[1], {
    serial: "192.168.1.80:5555",
    model: "Pixel 7",
    isWireless: true,
  });
});

test("bundled android-daemon.jar exists and is a valid ZIP/DEX archive", () => {
  const jarPath = bundledDaemonJarPath();
  assert.ok(jarPath, "Expected bundled android-daemon.jar to resolve");
  assert.equal(existsSync(jarPath), true);
  const bytes = readFileSync(jarPath);
  // ZIP magic bytes PK\x03\x04
  assert.equal(bytes[0], 0x50);
  assert.equal(bytes[1], 0x4b);
  assert.equal(bytes[2], 0x03);
  assert.equal(bytes[3], 0x04);
  assert.ok(bytes.length > 100_000, "Expected compiled single-DEX JAR > 100KB");
});

test("deployDaemon caches SHA-256 push, sets adb forward, launches app_process and checks READY", async () => {
  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "droidpath-cli-deploy-"));
  const fakeJar = path.join(tmpDir, "android-daemon.jar");
  writeFileSync(fakeJar, Buffer.from("PK\x03\x04fake-dex-payload"));
  const expectedSha = sha256File(fakeJar);

  // Case 1: Remote SHA empty -> must adb push, forward, launch, and report READY
  const calls1 = [];
  const res1 = await deployDaemon({
    serial: "61061JEA307110",
    port: 27189,
    jarPath: fakeJar,
    adbPath: "/fake/adb",
    execAdb: (args) => {
      calls1.push(args);
      const joined = args.join(" ");
      if (joined.includes("sha256sum")) return "";
      if (joined.includes("SHADOWGRAPH_DAEMON_READY")) return "READY\n";
      return "";
    },
  });
  assert.equal(res1.pushed, true);
  assert.equal(res1.ready, true);
  assert.equal(res1.sha256, expectedSha);
  assert.ok(calls1.some((c) => c.includes("push")));
  assert.ok(calls1.some((c) => c.includes("forward") && c.includes("tcp:27189")));
  assert.ok(calls1.some((c) => c.join(" ").includes("com.shadowgraph.daemon.DaemonMain")));

  // Case 2: Remote SHA matches -> must SKIP adb push!
  const calls2 = [];
  const res2 = await deployDaemon({
    serial: "61061JEA307110",
    port: 27189,
    jarPath: fakeJar,
    adbPath: "/fake/adb",
    execAdb: (args) => {
      calls2.push(args);
      const joined = args.join(" ");
      if (joined.includes("sha256sum")) return `${expectedSha}  /data/local/tmp/android-daemon.jar\n`;
      if (joined.includes("SHADOWGRAPH_DAEMON_READY")) return "READY\n";
      return "";
    },
  });
  assert.equal(res2.pushed, false);
  assert.equal(res2.ready, true);
  assert.ok(!calls2.some((c) => c.includes("push")), "Should skip adb push when SHA-256 matches");

  // Case 3: Fatal startup error surfaces immediately with remote log
  await assert.rejects(
    () =>
      deployDaemon({
        serial: "61061JEA307110",
        port: 27189,
        jarPath: fakeJar,
        adbPath: "/fake/adb",
        execAdb: (args) => {
          const joined = args.join(" ");
          if (joined.includes("sha256sum")) return `${expectedSha}  /data/local/tmp/android-daemon.jar`;
          if (joined.includes("SHADOWGRAPH_DAEMON_READY")) return "FAILED\n";
          if (joined.includes("cat ")) return "Fatal startup error: BindException\n";
          return "";
        },
      }),
    /BindException/
  );

  rmSync(tmpDir, { recursive: true, force: true });
});

test("configureMcpClient safely merges Cursor & Claude Desktop configs across platforms and refuses corrupt JSON", () => {
  // Verify cross-platform config path resolution
  assert.equal(
    resolveClientConfigPath("claude", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/Library/Application Support/Claude/claude_desktop_config.json"
  );
  assert.equal(
    resolveClientConfigPath("claude", {
      platform: "win32",
      homeDir: "C:\\Users\\dev",
      env: { APPDATA: "C:\\Users\\dev\\AppData\\Roaming" },
    }),
    path.win32.join("C:\\Users\\dev\\AppData\\Roaming", "Claude", "claude_desktop_config.json")
  );
  assert.equal(
    resolveClientConfigPath("claude", { platform: "linux", homeDir: "/home/dev" }),
    "/home/dev/.config/Claude/claude_desktop_config.json"
  );

  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "droidpath-mcp-cfg-"));
  const cfgPath = path.join(tmpDir, "mcp.json");

  // Pre-existing MCP server must be preserved!
  writeFileSync(
    cfgPath,
    JSON.stringify({ mcpServers: { existing: { command: "echo", args: ["hi"] } }, customKey: 42 })
  );

  const entry = buildMcpServerEntry({
    mode: "stdio",
    binaryPath: "/home/dev/.droidpath/bin/droidpath-mcp",
    jarPath: "/home/dev/.droidpath/share/android-daemon.jar",
  });

  // Dry run must NOT modify disk
  const dry = configureMcpClient({ client: "cursor", configPath: cfgPath, entry, dryRun: true });
  assert.equal(dry.changed, true);
  assert.equal(JSON.parse(readFileSync(cfgPath, "utf8")).mcpServers.droidpath, undefined);

  // Real write merges atomically
  const applied = configureMcpClient({ client: "cursor", configPath: cfgPath, entry, dryRun: false });
  assert.equal(applied.changed, true);
  const saved = JSON.parse(readFileSync(cfgPath, "utf8"));
  assert.equal(saved.customKey, 42);
  assert.deepEqual(saved.mcpServers.existing, { command: "echo", args: ["hi"] });
  assert.deepEqual(saved.mcpServers.droidpath, entry);

  // Second apply is idempotent
  const second = configureMcpClient({ client: "cursor", configPath: cfgPath, entry, dryRun: false });
  assert.equal(second.changed, false);

  // Corrupt JSON must NOT be overwritten
  const badPath = path.join(tmpDir, "corrupt.json");
  writeFileSync(badPath, "{ not valid json");
  assert.throws(
    () => configureMcpClient({ client: "cursor", configPath: badPath, entry }),
    /Refusing to overwrite invalid JSON/
  );

  rmSync(tmpDir, { recursive: true, force: true });
});

test("releaseAssetName covers all 6 desktop targets and CLI parses flags cleanly", () => {
  assert.equal(releaseAssetName("darwin", "arm64"), "droidpath-mcp-aarch64-apple-darwin");
  assert.equal(releaseAssetName("darwin", "x64"), "droidpath-mcp-x86_64-apple-darwin");
  assert.equal(releaseAssetName("linux", "x64"), "droidpath-mcp-x86_64-unknown-linux-gnu");
  assert.equal(releaseAssetName("linux", "arm64"), "droidpath-mcp-aarch64-unknown-linux-gnu");
  assert.equal(releaseAssetName("win32", "x64"), "droidpath-mcp-x86_64-pc-windows-msvc.exe");
  assert.equal(releaseAssetName("win32", "arm64"), "droidpath-mcp-aarch64-pc-windows-msvc.exe");

  const parsed = parseCliArgs(["init", "--client", "cursor", "--http", "--port", "9000", "--dry-run"]);
  assert.deepEqual(parsed.positional, ["init"]);
  assert.equal(parsed.flags.client, "cursor");
  assert.equal(parsed.flags.http, true);
  assert.equal(parsed.flags.port, 9000);
  assert.equal(parsed.flags.dryRun, true);
});

test("runCli --version, --help, and doctor --json succeed cleanly", async () => {
  const lines = [];
  const io = { out: (s) => lines.push(s), err: () => {} };

  assert.equal(await runCli(["--version"], io), 0);
  assert.match(lines[0], /droidpath 2\.1\.0/);

  lines.length = 0;
  assert.equal(await runCli(["doctor", "--json"], io), 0);
  const report = JSON.parse(lines.join("\n"));
  assert.equal(report.cliVersion, "2.1.0");
  assert.equal(report.daemonJar.found, true);
});

test("checkTcpPort distinguishes live NDJSON android-daemon from stale adb forward EOF tunnel", async () => {
  // 1. Stale adb forward simulation: accepts TCP and immediately closes (EOF with 0 bytes)
  const staleServer = net.createServer((sock) => {
    sock.on("error", () => {});
    sock.end();
  });
  await new Promise((r) => staleServer.listen(0, "127.0.0.1", r));
  const stalePort = staleServer.address().port;
  assert.equal(await checkTcpPort("127.0.0.1", stalePort, 300), false);
  staleServer.close();

  // 2. Live android-daemon simulation: responds with NDJSON frame containing '{'
  const liveServer = net.createServer((sock) => {
    sock.on("error", () => {});
    sock.on("data", () => {
      sock.end('{"type":"pong","seq":1}\n');
    });
  });
  await new Promise((r) => liveServer.listen(0, "127.0.0.1", r));
  const livePort = liveServer.address().port;
  assert.equal(await checkTcpPort("127.0.0.1", livePort, 300), true);
  liveServer.close();
});

test("supports Gemini CLI, Antigravity, LM Studio, Ollama/OpenWebUI/Local AI config paths and local HTTP manifest", () => {
  assert.equal(
    resolveClientConfigPath("gemini", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/.gemini/settings.json"
  );
  assert.equal(
    resolveClientConfigPath("gemini-cli", { platform: "linux", homeDir: "/home/dev" }),
    "/home/dev/.gemini/settings.json"
  );
  assert.equal(
    resolveClientConfigPath("antigravity", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/.gemini/antigravity/mcp_config.json"
  );
  assert.equal(
    resolveClientConfigPath("lm-studio", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/.lmstudio/mcp.json"
  );
  assert.equal(
    resolveClientConfigPath("local", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/.droidpath/local-ai/mcp-http.json"
  );
  assert.equal(
    resolveClientConfigPath("ollama", { platform: "darwin", homeDir: "/Users/dev" }),
    "/Users/dev/.droidpath/local-ai/mcp-http.json"
  );

  assert.deepEqual(expandClients("all"), [
    "cursor",
    "claude",
    "claude-code",
    "gemini",
    "antigravity",
    "lm-studio",
    "local",
  ]);

  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "droidpath-local-ai-"));
  const localCfgPath = path.join(tmpDir, "mcp-http.json");
  const entry = buildMcpServerEntry({ mode: "stdio" });
  const localRes = configureMcpClient({
    client: "local",
    configPath: localCfgPath,
    entry,
    port: 8766,
  });
  assert.equal(localRes.changed, true);
  const parsed = JSON.parse(readFileSync(localCfgPath, "utf8"));
  assert.equal(parsed.mcpServers.droidpath.url, "http://127.0.0.1:8766/sse");
  assert.equal(parsed.localAi.loopbackOnly, true);
  assert.equal(parsed.localAi.messageUrl, "http://127.0.0.1:8766/message");

  // Local AI bundle exports compact system prompt + OpenAI tool schemas
  const bundle = installLocalAiBundle({ outDir: tmpDir, port: 8766, dryRun: false });
  assert.equal(bundle.length, 2);
  assert.equal(bundle[0].changed, true);
  const toolsJson = JSON.parse(readFileSync(path.join(tmpDir, "openai-tools.json"), "utf8"));
  assert.equal(toolsJson.server.loopbackOnly, true);
  const navFn = toolsJson.tools.find((t) => t.function.name === "android_navigate_to_node");
  const expFn = toolsJson.tools.find((t) => t.function.name === "android_explore_and_click");
  assert.ok(navFn?.function.parameters.properties.auto_explore);
  assert.ok(expFn?.function.parameters.properties.target);
  assert.ok(buildOpenAIToolSchemas().length >= 9);
  assert.match(buildCompactLocalSystemPrompt(), /android_navigate_to_node/);
  assert.match(buildCompactLocalSystemPrompt(), /requires_confirmation/);
  assert.match(buildCompactLocalSystemPrompt(), /secure_screen_active/);

  rmSync(tmpDir, { recursive: true, force: true });
});

test("installAgentRulesFile merges idempotent managed blocks without clobbering user content", () => {
  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "droidpath-rules-"));
  const rulesFile = path.join(tmpDir, "GEMINI.md");

  // 1. Existing user content before and after must stay intact
  writeFileSync(rulesFile, "# My Custom Project Rules\nKeep TypeScript strict.\n", "utf8");

  // Dry-run does not modify file
  const dry = installAgentRulesFile({ filePath: rulesFile, content: "Rule V1", dryRun: true });
  assert.equal(dry.changed, true);
  assert.equal(dry.action, "appended");
  assert.equal(readFileSync(rulesFile, "utf8").includes(RULES_START_MARKER), false);

  // Append managed block
  const r1 = installAgentRulesFile({ filePath: rulesFile, content: "Rule V1", dryRun: false });
  assert.equal(r1.changed, true);
  assert.equal(r1.action, "appended");
  const afterAppend = readFileSync(rulesFile, "utf8");
  assert.match(afterAppend, /# My Custom Project Rules/);
  assert.match(afterAppend, /Keep TypeScript strict\./);
  assert.ok(afterAppend.includes(`${RULES_START_MARKER}\nRule V1\n${RULES_END_MARKER}`));

  // Append trailing user content below block, then update managed block -> trailing text preserved!
  writeFileSync(rulesFile, `${afterAppend}\n## User Footer\nDo not delete me.\n`, "utf8");
  const r2 = installAgentRulesFile({ filePath: rulesFile, content: "Rule V2", dryRun: false });
  assert.equal(r2.changed, true);
  assert.equal(r2.action, "updated");
  const afterUpdate = readFileSync(rulesFile, "utf8");
  assert.match(afterUpdate, /# My Custom Project Rules/);
  assert.match(afterUpdate, /## User Footer\nDo not delete me\./);
  assert.ok(afterUpdate.includes("Rule V2"));
  assert.ok(!afterUpdate.includes("Rule V1"));

  // Re-running with same content is a pure no-op
  const r3 = installAgentRulesFile({ filePath: rulesFile, content: "Rule V2", dryRun: false });
  assert.equal(r3.changed, false);
  assert.equal(r3.action, "unchanged");

  // Malformed markers must refuse to overwrite
  const badFile = path.join(tmpDir, "BROKEN.md");
  writeFileSync(badFile, `Header\n${RULES_START_MARKER}\nUnclosed block\n`, "utf8");
  assert.throws(
    () => installAgentRulesFile({ filePath: badFile, content: "Rule V3" }),
    /Malformed DroidPath rules markers/
  );

  // Verify bundled canonical rules asset loads cleanly
  const canonical = loadCanonicalAgentRules();
  assert.match(canonical, /android_navigate_to_node/);
  assert.match(canonical, /android_explore_and_click/);
  assert.match(canonical, /android_screenshot/);
  assert.match(canonical, /requires_confirmation/);
  assert.match(canonical, /secure_screen_active/);

  rmSync(tmpDir, { recursive: true, force: true });
});

test("CLI rules command, --format exports, and init --no-rules flag work cleanly", async () => {
  const parsedNoRules = parseCliArgs(["init", "--client", "gemini,antigravity", "--no-rules", "--dry-run"]);
  assert.equal(parsedNoRules.flags.noRules, true);
  assert.equal(parsedNoRules.flags.client, "gemini,antigravity");

  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "droidpath-cli-rules-"));
  const lines = [];
  const io = { out: (s) => lines.push(s), err: (s) => lines.push(s) };

  // 1. `droidpath rules --dir <tmpDir> --client all --json`
  assert.equal(await runCli(["rules", "--dir", tmpDir, "--client", "all", "--json"], io), 0);
  const rulesReport = JSON.parse(lines.join("\n"));
  assert.ok(rulesReport.rules.length >= 6);
  assert.equal(existsSync(path.join(tmpDir, ".cursorrules")), true);
  assert.equal(existsSync(path.join(tmpDir, "CLAUDE.md")), true);
  assert.equal(existsSync(path.join(tmpDir, "GEMINI.md")), true);
  assert.equal(existsSync(path.join(tmpDir, ".agent", "rules", "droidpath.md")), true);
  assert.equal(existsSync(path.join(tmpDir, ".droidpath-local-ai", "system-prompt.md")), true);
  assert.equal(existsSync(path.join(tmpDir, ".droidpath-local-ai", "openai-tools.json")), true);

  // 2. `droidpath rules --format openai` prints valid OpenAI function schemas
  lines.length = 0;
  assert.equal(await runCli(["rules", "--format", "openai"], io), 0);
  const schemas = JSON.parse(lines.join("\n"));
  assert.ok(Array.isArray(schemas));
  assert.ok(schemas.some((t) => t.function.name === "android_explore_and_click"));

  // 3. `droidpath rules --format prompt`, `local`, and `system-prompt` print compact local system prompt
  for (const fmt of ["prompt", "local", "system-prompt"]) {
    lines.length = 0;
    assert.equal(await runCli(["rules", "--format", fmt], io), 0);
    const out = lines.join("\n");
    assert.match(out, /android_navigate_to_node/);
    assert.match(out, /android_explore_and_click/);
  }

  // 4. `droidpath init --dry-run --no-rules --json` configures MCP entries with zero rules
  lines.length = 0;
  assert.equal(
    await runCli(["init", "--client", "cursor,gemini,antigravity,local", "--no-rules", "--dry-run", "--json"], io),
    0
  );
  const initNoRules = JSON.parse(lines.join("\n"));
  assert.equal(initNoRules.configured.length, 4);
  assert.equal(initNoRules.rules.length, 0);

  // 5. `droidpath init --dry-run --dir <tmpDir2> --json` installs rules by default
  const tmpDir2 = mkdtempSync(path.join(os.tmpdir(), "droidpath-cli-init-default-"));
  lines.length = 0;
  assert.equal(
    await runCli(["init", "--client", "cursor,gemini,antigravity,local", "--dir", tmpDir2, "--dry-run", "--json"], io),
    0
  );
  const initWithRules = JSON.parse(lines.join("\n"));
  assert.equal(initWithRules.configured.length, 4);
  assert.ok(initWithRules.rules.length >= 5, "init must install rules by default");

  // Verify resolveClientRulesPaths global mode
  const globalGemini = resolveClientRulesPaths("gemini", {
    projectDir: tmpDir,
    homeDir: "/Users/dev",
    globalInstall: true,
    platform: "darwin",
  });
  assert.ok(globalGemini.includes("/Users/dev/.gemini/GEMINI.md"));

  rmSync(tmpDir, { recursive: true, force: true });
  rmSync(tmpDir2, { recursive: true, force: true });
});
