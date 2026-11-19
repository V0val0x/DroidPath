import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export const REMOTE_DAEMON_JAR = "/data/local/tmp/android-daemon.jar";
export const REMOTE_DAEMON_LOG = "/data/local/tmp/android-daemon.log";

export function bundledDaemonJarPath() {
  const candidates = [
    process.env.DROIDPATH_DAEMON_JAR,
    path.resolve(__dirname, "../assets/android-daemon.jar"),
    path.resolve(__dirname, "../../../android-daemon/build/dist/android-daemon.jar"),
    path.join(os.homedir(), ".droidpath", "share", "android-daemon.jar"),
  ].filter(Boolean);
  return candidates.find((p) => existsSync(p)) ?? null;
}

export function validateSerial(serial) {
  if (typeof serial !== "string") return false;
  const trimmed = serial.trim();
  if (!trimmed || trimmed.length > 128 || trimmed.startsWith("-")) return false;
  return /^[A-Za-z0-9.:_-]+$/.test(trimmed);
}

export function resolveAdbPath(env = process.env, platform = process.platform) {
  const isWin = platform === "win32";
  const bin = isWin ? "adb.exe" : "adb";
  const candidates = [];

  if (env.DROIDPATH_ADB_PATH) candidates.push(env.DROIDPATH_ADB_PATH);
  if (env.ANDROID_HOME) candidates.push(path.join(env.ANDROID_HOME, "platform-tools", bin));
  if (env.ANDROID_SDK_ROOT) candidates.push(path.join(env.ANDROID_SDK_ROOT, "platform-tools", bin));

  if (isWin) {
    if (env.LOCALAPPDATA) {
      candidates.push(path.join(env.LOCALAPPDATA, "Android", "Sdk", "platform-tools", "adb.exe"));
    }
    candidates.push(path.join(os.homedir(), "AppData", "Local", "Android", "Sdk", "platform-tools", "adb.exe"));
    candidates.push(path.join(os.homedir(), "scoop", "shims", "adb.exe"));
    candidates.push("C:\\platform-tools\\adb.exe");
  } else if (platform === "darwin") {
    candidates.push(path.join(os.homedir(), "Library", "Android", "sdk", "platform-tools", "adb"));
    candidates.push("/opt/homebrew/bin/adb");
    candidates.push("/usr/local/bin/adb");
  } else {
    candidates.push(path.join(os.homedir(), "Android", "Sdk", "platform-tools", "adb"));
    candidates.push("/usr/bin/adb");
    candidates.push("/usr/local/bin/adb");
  }

  if (env.PATH) {
    for (const dir of env.PATH.split(path.delimiter)) {
      if (dir) candidates.push(path.join(dir, bin));
    }
  }

  for (const cand of candidates) {
    try {
      if (existsSync(cand) && statSync(cand).isFile()) {
        return cand;
      }
    } catch {
      // Ignore unreadable path entries
    }
  }
  return null;
}

export function runAdb(args, { adbPath = resolveAdbPath(), timeoutMs = 8000 } = {}) {
  if (!adbPath) {
    throw new Error(
      "ADB executable not found. Install android-platform-tools or set ANDROID_HOME / DROIDPATH_ADB_PATH."
    );
  }
  const res = spawnSync(adbPath, args, {
    encoding: "utf8",
    timeout: timeoutMs,
    windowsHide: true,
  });
  if (res.error) {
    throw new Error(`Failed to invoke adb: ${res.error.message}`);
  }
  if (res.status !== 0) {
    const stderr = (res.stderr || res.stdout || "").trim();
    throw new Error(`adb ${args.join(" ")} failed (exit ${res.status}): ${stderr}`);
  }
  return res.stdout || "";
}

export function parseDevicesOutput(output) {
  const devices = [];
  for (const rawLine of output.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("List of devices") || line.startsWith("*")) continue;
    const parts = line.split(/\s+/);
    if (parts.length < 2 || parts[1] !== "device") continue;
    const serial = parts[0];
    if (!validateSerial(serial)) continue;
    const modelTok = parts.find((p) => p.startsWith("model:"));
    const model = modelTok ? modelTok.slice("model:".length).replaceAll("_", " ") : serial;
    const isWireless = serial.includes(":") || serial.includes("_adb-tls-connect");
    devices.push({ serial, model, isWireless });
  }
  return devices;
}

export function listDevices(opts = {}) {
  const out = runAdb(["devices", "-l"], { ...opts, timeoutMs: 4000 });
  return parseDevicesOutput(out);
}

export function resolveTargetSerial(requested, opts = {}) {
  if (requested && requested.trim()) {
    const trimmed = requested.trim();
    if (!validateSerial(trimmed)) {
      throw new Error(`Invalid ADB device serial '${trimmed}'.`);
    }
    return trimmed;
  }
  const devices = listDevices(opts);
  if (devices.length === 0) {
    throw new Error("No authorized Android devices found via `adb devices`. Connect a device and enable USB Debugging.");
  }
  return devices[0].serial;
}

export function sha256File(filePath) {
  const data = readFileSync(filePath);
  return createHash("sha256").update(data).digest("hex");
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function checkTcpPort(host, port, timeoutMs = 400) {
  return new Promise((resolve) => {
    const socket = net.createConnection({ host, port });
    let settled = false;
    const done = (ok) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(timeoutMs, () => done(false));
    socket.on("connect", () => {
      try {
        socket.write('{"cmd":"ping"}\n');
      } catch {
        done(false);
      }
    });
    socket.on("data", (chunk) => {
      if (Buffer.from(chunk).includes(0x7b)) {
        done(true);
      }
    });
    socket.on("end", () => done(false));
    socket.on("close", () => done(false));
    socket.on("error", () => done(false));
  });
}

export async function deployDaemon({
  serial,
  port = 27189,
  jarPath = bundledDaemonJarPath(),
  debounceMs = 20,
  maxDepth = 48,
  heartbeatMs = 5000,
  adbPath = resolveAdbPath(),
  execAdb = runAdb,
  logger = () => {},
} = {}) {
  const resolvedSerial = serial
    ? (() => {
        if (!validateSerial(serial)) throw new Error(`Invalid ADB serial '${serial}'`);
        return serial.trim();
      })()
    : resolveTargetSerial(undefined, { adbPath });

  let pushed = false;
  let sha256 = null;

  if (jarPath && existsSync(jarPath)) {
    sha256 = sha256File(jarPath);
    let remoteSha = "";
    try {
      const shaOut = execAdb(
        [
          "-s",
          resolvedSerial,
          "shell",
          `sha256sum '${REMOTE_DAEMON_JAR}' 2>/dev/null || toybox sha256sum '${REMOTE_DAEMON_JAR}' 2>/dev/null`,
        ],
        { adbPath, timeoutMs: 3000 }
      );
      remoteSha = (shaOut.trim().split(/\s+/)[0] || "").trim();
    } catch {
      remoteSha = "";
    }

    if (remoteSha !== sha256) {
      logger(`Pushing android-daemon.jar -> ${REMOTE_DAEMON_JAR} (${resolvedSerial})...`);
      execAdb(["-s", resolvedSerial, "push", jarPath, REMOTE_DAEMON_JAR], {
        adbPath,
        timeoutMs: 15000,
      });
      try {
        execAdb(["-s", resolvedSerial, "shell", "chmod", "644", REMOTE_DAEMON_JAR], {
          adbPath,
          timeoutMs: 3000,
        });
      } catch {
        // Ignore chmod errors on non-standard shells
      }
      pushed = true;
    } else {
      logger(`Remote android-daemon.jar checksum matches (${sha256.slice(0, 12)}...); skipping push.`);
    }
  } else {
    const checkOut = execAdb(
      ["-s", resolvedSerial, "shell", `test -f '${REMOTE_DAEMON_JAR}' && echo OK`],
      { adbPath, timeoutMs: 3000 }
    );
    if (!checkOut.includes("OK")) {
      throw new Error(`android-daemon.jar not found locally or at ${REMOTE_DAEMON_JAR}.`);
    }
  }

  // Stop any existing daemon instance cleanly
  const killCmd =
    "pkill -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true; " +
    "i=0; while [ $i -lt 15 ]; do pgrep -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || break; sleep 0.05; i=$((i+1)); done; " +
    "pkill -9 -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true";
  try {
    execAdb(["-s", resolvedSerial, "shell", killCmd], { adbPath, timeoutMs: 4000 });
  } catch {
    // Ignore if no previous daemon existed
  }

  // Forward loopback TCP port
  execAdb(["-s", resolvedSerial, "forward", `tcp:${port}`, `tcp:${port}`], {
    adbPath,
    timeoutMs: 3000,
  });

  // Launch detached via app_process
  const daemonCmd =
    `env CLASSPATH='${REMOTE_DAEMON_JAR}' app_process / com.shadowgraph.daemon.DaemonMain ` +
    `--port=${port} --debounce-ms=${debounceMs} --max-depth=${maxDepth} --heartbeat-ms=${heartbeatMs} --no-stdin-watchdog`;
  const launchCmd =
    `rm -f '${REMOTE_DAEMON_LOG}'; ` +
    `if command -v setsid >/dev/null 2>&1; then ` +
    `(setsid ${daemonCmd} </dev/null >'${REMOTE_DAEMON_LOG}' 2>&1 &) </dev/null >/dev/null 2>&1; ` +
    `else (nohup ${daemonCmd} </dev/null >'${REMOTE_DAEMON_LOG}' 2>&1 &) </dev/null >/dev/null 2>&1; fi`;

  execAdb(["-s", resolvedSerial, "shell", launchCmd], { adbPath, timeoutMs: 4000 });

  const checkCmd =
    `if grep -q 'SHADOWGRAPH_DAEMON_READY' '${REMOTE_DAEMON_LOG}' 2>/dev/null; then echo READY; ` +
    `elif grep -q 'Fatal startup error' '${REMOTE_DAEMON_LOG}' 2>/dev/null; then echo FAILED; fi`;

  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    let statusOut = "";
    try {
      statusOut = execAdb(["-s", resolvedSerial, "shell", checkCmd], {
        adbPath,
        timeoutMs: 2000,
      });
    } catch {
      statusOut = "";
    }
    if (statusOut.includes("READY")) {
      return {
        serial: resolvedSerial,
        port,
        jarPath,
        sha256,
        pushed,
        ready: true,
      };
    }
    if (statusOut.includes("FAILED")) {
      const log = execAdb(["-s", resolvedSerial, "shell", `cat '${REMOTE_DAEMON_LOG}' 2>/dev/null`], {
        adbPath,
        timeoutMs: 2000,
      });
      throw new Error(`android-daemon failed to start:\n${log}`);
    }
    await sleep(100);
  }

  let tailLog = "";
  try {
    tailLog = execAdb(["-s", resolvedSerial, "shell", `cat '${REMOTE_DAEMON_LOG}' 2>/dev/null`], {
      adbPath,
      timeoutMs: 2000,
    }).trim();
  } catch {
    // Ignore
  }
  throw new Error(
    `Timed out waiting for SHADOWGRAPH_DAEMON_READY.${tailLog ? ` Remote log: ${tailLog}` : ""}`
  );
}

export function stopDaemon({
  serial,
  port = 27189,
  adbPath = resolveAdbPath(),
  execAdb = runAdb,
} = {}) {
  const resolvedSerial = resolveTargetSerial(serial, { adbPath });
  const killCmd =
    "pkill -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true; " +
    "pkill -9 -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true";
  try {
    execAdb(["-s", resolvedSerial, "shell", killCmd], { adbPath, timeoutMs: 4000 });
  } catch {
    // Ignore
  }
  try {
    execAdb(["-s", resolvedSerial, "forward", "--remove", `tcp:${port}`], {
      adbPath,
      timeoutMs: 2000,
    });
  } catch {
    // Ignore
  }
  return { serial: resolvedSerial, port, stopped: true };
}
