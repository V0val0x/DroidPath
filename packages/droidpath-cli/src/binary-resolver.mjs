import { chmodSync, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_SLUG = "V0val0x/DroidPath";

export function releaseAssetName(platform = process.platform, arch = process.arch) {
  if (platform === "darwin" && arch === "arm64") return "droidpath-mcp-aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "droidpath-mcp-x86_64-apple-darwin";
  if (platform === "linux" && arch === "x64") return "droidpath-mcp-x86_64-unknown-linux-gnu";
  if (platform === "linux" && arch === "arm64") return "droidpath-mcp-aarch64-unknown-linux-gnu";
  if (platform === "win32" && arch === "x64") return "droidpath-mcp-x86_64-pc-windows-msvc.exe";
  if (platform === "win32" && arch === "arm64") return "droidpath-mcp-aarch64-pc-windows-msvc.exe";
  return null;
}

export function resolveMcpBinaryPath({ env = process.env, platform = process.platform } = {}) {
  const exe = platform === "win32" ? "droidpath-mcp.exe" : "droidpath-mcp";
  const candidates = [];

  if (env.DROIDPATH_MCP_BIN) candidates.push(env.DROIDPATH_MCP_BIN);
  candidates.push(path.join(os.homedir(), ".droidpath", "bin", exe));
  candidates.push(path.resolve(__dirname, `../../../target/release/${exe}`));
  candidates.push(path.resolve(__dirname, `../../../target/debug/${exe}`));

  if (env.PATH) {
    for (const dir of env.PATH.split(path.delimiter)) {
      if (dir) candidates.push(path.join(dir, exe));
    }
  }

  for (const cand of candidates) {
    try {
      if (existsSync(cand) && statSync(cand).isFile()) {
        return cand;
      }
    } catch {
      // Ignore
    }
  }
  return null;
}

export function resolveDesktopHudBinaryPath({ platform = process.platform } = {}) {
  const candidates = [];
  if (platform === "darwin") {
    candidates.push("/Applications/DroidPath.app/Contents/MacOS/DroidPath");
    candidates.push(path.resolve(__dirname, "../../../target/release/bundle/macos/DroidPath.app/Contents/MacOS/DroidPath"));
    candidates.push(path.resolve(__dirname, "../../../target/release/droidpath-desktop"));
  } else if (platform === "win32") {
    candidates.push(path.join(os.homedir(), ".droidpath", "bin", "droidpath-desktop.exe"));
    candidates.push(path.resolve(__dirname, "../../../target/release/droidpath-desktop.exe"));
  } else {
    candidates.push(path.join(os.homedir(), ".droidpath", "bin", "droidpath-desktop"));
    candidates.push(path.resolve(__dirname, "../../../target/release/droidpath-desktop"));
  }
  return candidates.find((p) => existsSync(p)) ?? null;
}

export async function downloadNativeBinary({ version = "latest", logger = () => {} } = {}) {
  const asset = releaseAssetName();
  if (!asset) {
    throw new Error(`No prebuilt droidpath-mcp binary for ${process.platform}/${process.arch}. Build with: cargo build --release -p droidpath-mcp`);
  }
  const url =
    version === "latest"
      ? `https://github.com/${REPO_SLUG}/releases/latest/download/${asset}`
      : `https://github.com/${REPO_SLUG}/releases/download/${version}/${asset}`;

  logger(`Downloading ${asset} from GitHub Releases...`);
  const resp = await fetch(url, { redirect: "follow" });
  if (!resp.ok) {
    throw new Error(`Failed to download ${url}: HTTP ${resp.status}`);
  }
  const buf = Buffer.from(await resp.arrayBuffer());
  const binDir = path.join(os.homedir(), ".droidpath", "bin");
  mkdirSync(binDir, { recursive: true });
  const exeName = process.platform === "win32" ? "droidpath-mcp.exe" : "droidpath-mcp";
  const dest = path.join(binDir, exeName);
  writeFileSync(dest, buf);
  if (process.platform !== "win32") {
    chmodSync(dest, 0o755);
  }
  return dest;
}
