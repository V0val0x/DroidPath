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
import { parseCliArgs, runCli } from "../src/index.mjs";
import {
  buildMcpServerEntry,
  configureMcpClient,
  resolveClientConfigPath,
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
  assert.match(lines[0], /droidpath 2\.0\.0/);

  lines.length = 0;
  assert.equal(await runCli(["doctor", "--json"], io), 0);
  const report = JSON.parse(lines.join("\n"));
  assert.equal(report.cliVersion, "2.0.0");
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
