import { createHash, timingSafeEqual } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
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

/**
 * Parses a GNU `sha256sum` / macOS `shasum -a 256` manifest (`checksums.txt`).
 * Supports both `<hex>  <filename>` and `<hex> *<filename>`, optional `./` prefix,
 * CRLF line endings, and filenames containing spaces.
 */
export function parseChecksumsManifest(text) {
  const map = new Map();
  if (typeof text !== "string") return map;

  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const match = /^([0-9a-fA-F]{64})\s+\*?(.+)$/.exec(line);
    if (!match) continue;
    const digest = match[1].toLowerCase();
    const rawName = match[2].trim();
    const normalizedName = rawName.replace(/^\.\//, "");
    if (!normalizedName) continue;
    map.set(normalizedName, digest);
    const base = path.posix.basename(normalizedName);
    if (base && !map.has(base)) {
      map.set(base, digest);
    }
  }
  return map;
}

/**
 * Computes SHA-256 of `buffer` and compares it in constant time against `expectedHex`.
 * Throws an Error on malformed hash or digest mismatch.
 */
export function verifySha256Buffer(buffer, expectedHex, assetName = "artifact") {
  if (typeof expectedHex !== "string" || !/^[0-9a-fA-F]{64}$/.test(expectedHex)) {
    throw new Error(`Invalid expected SHA-256 digest for ${assetName}.`);
  }
  const actualHex = createHash("sha256").update(buffer).digest("hex");
  const expectedBuf = Buffer.from(expectedHex.toLowerCase(), "hex");
  const actualBuf = Buffer.from(actualHex, "hex");
  if (expectedBuf.length !== actualBuf.length || !timingSafeEqual(expectedBuf, actualBuf)) {
    throw new Error(
      `SHA-256 verification failed for ${assetName}: expected ${expectedHex.toLowerCase()}, got ${actualHex}. Refusing to install tampered or corrupted binary.`
    );
  }
  return actualHex;
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

async function fetchChecksumsWithPinnedBase({ initialBaseUrl, fetchImpl, logger }) {
  const MAX_REDIRECTS = 10;
  const initialUrlObj = new URL(`${initialBaseUrl}/checksums.txt`);
  const initialOrigin = initialUrlObj.origin;
  let currentUrl = initialUrlObj.href;
  let pinnedBase = initialBaseUrl;

  logger(`Fetching SHA-256 manifest from ${currentUrl}...`);

  let resp = null;
  for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
    resp = await fetchImpl(currentUrl, { redirect: "manual" });

    if (resp.status >= 300 && resp.status < 400) {
      const location = resp.headers?.get?.("location");
      if (!location) {
        throw new Error(
          `Redirect (${resp.status}) without Location header while fetching ${currentUrl}.`
        );
      }
      if (hop === MAX_REDIRECTS) {
        throw new Error(`Too many redirects while fetching SHA-256 checksums.txt (${currentUrl}).`);
      }
      const nextUrl = new URL(location, currentUrl);
      // Pin release tag base ONLY on same-origin release redirects ending in /checksums.txt
      // (e.g. /releases/latest/download/checksums.txt -> /releases/download/v2.1.2/checksums.txt),
      // never to third-party/object-storage CDN URLs (e.g. release-assets.githubusercontent.com/<uuid>).
      if (nextUrl.origin === initialOrigin && nextUrl.pathname.endsWith("/checksums.txt")) {
        pinnedBase = `${nextUrl.origin}${nextUrl.pathname.slice(0, -"/checksums.txt".length)}`.replace(/\/+$/, "");
      }
      currentUrl = nextUrl.href;
      continue;
    }
    break;
  }

  if (!resp || !resp.ok) {
    const status = resp ? resp.status : "unknown";
    throw new Error(
      `Failed to fetch SHA-256 checksums.txt (${initialUrlObj.href}: HTTP ${status}). Refusing unverified binary download (pass --allow-unverified or set DROIDPATH_ALLOW_UNVERIFIED=1 to override).`
    );
  }

  // Fallback for custom fetchImpl mocks that auto-followed redirects despite redirect: "manual"
  if (resp.url) {
    try {
      const finalUrl = new URL(resp.url);
      if (finalUrl.origin === initialOrigin && finalUrl.pathname.endsWith("/checksums.txt")) {
        pinnedBase = `${finalUrl.origin}${finalUrl.pathname.slice(0, -"/checksums.txt".length)}`.replace(/\/+$/, "");
      }
    } catch {
      // Keep pinnedBase as-is if custom fetchImpl omits a parseable URL
    }
  }

  const manifestText = await resp.text();
  return { manifestText, pinnedBase };
}

export async function downloadNativeBinary({
  version = "latest",
  baseUrl = null,
  destDir = null,
  platform = process.platform,
  arch = process.arch,
  allowUnverified = false,
  env = process.env,
  fetchImpl = fetch,
  logger = () => {},
} = {}) {
  let asset = releaseAssetName(platform, arch);
  if (!asset) {
    throw new Error(`No prebuilt droidpath-mcp binary for ${platform}/${arch}. Build with: cargo build --release -p droidpath-mcp`);
  }
  const winArmFallbackAsset =
    platform === "win32" && arch === "arm64"
      ? "droidpath-mcp-x86_64-pc-windows-msvc.exe"
      : null;

  const effectiveAllowUnverified =
    Boolean(allowUnverified) || env.DROIDPATH_ALLOW_UNVERIFIED === "1";

  const rawBase =
    baseUrl ||
    env.DROIDPATH_RELEASE_BASE_URL ||
    (version === "latest"
      ? `https://github.com/${REPO_SLUG}/releases/latest/download`
      : `https://github.com/${REPO_SLUG}/releases/download/${version}`);
  let resolvedBase = rawBase.replace(/\/+$/, "");

  let expectedSha256 = null;
  if (!effectiveAllowUnverified) {
    const { manifestText, pinnedBase } = await fetchChecksumsWithPinnedBase({
      initialBaseUrl: resolvedBase,
      fetchImpl,
      logger,
    });
    resolvedBase = pinnedBase;
    const checksums = parseChecksumsManifest(manifestText);
    expectedSha256 = checksums.get(asset) ?? null;
    if (!expectedSha256 && winArmFallbackAsset && checksums.has(winArmFallbackAsset)) {
      logger(`Windows ARM64 binary not listed in checksums.txt; falling back to ${winArmFallbackAsset}...`);
      asset = winArmFallbackAsset;
      expectedSha256 = checksums.get(asset);
    }
    if (!expectedSha256) {
      throw new Error(
        `Asset '${asset}' is missing from checksums.txt. Refusing unverified binary download (pass --allow-unverified or set DROIDPATH_ALLOW_UNVERIFIED=1 to override).`
      );
    }
  } else {
    logger("⚠️  SHA-256 verification bypassed (--allow-unverified / DROIDPATH_ALLOW_UNVERIFIED=1).");
  }

  let url = `${resolvedBase}/${asset}`;
  logger(`Downloading ${asset} from ${url}...`);
  let resp = await fetchImpl(url, { redirect: "follow" });
  if (!resp.ok && effectiveAllowUnverified && winArmFallbackAsset && asset !== winArmFallbackAsset) {
    logger(`ARM64 download failed (HTTP ${resp.status}); falling back to ${winArmFallbackAsset}...`);
    asset = winArmFallbackAsset;
    url = `${resolvedBase}/${asset}`;
    resp = await fetchImpl(url, { redirect: "follow" });
  }
  if (!resp.ok) {
    throw new Error(`Failed to download ${url}: HTTP ${resp.status}`);
  }
  const buf = Buffer.from(await resp.arrayBuffer());

  if (expectedSha256) {
    const verified = verifySha256Buffer(buf, expectedSha256, asset);
    logger(`✅ Verified SHA-256 for ${asset} (${verified.slice(0, 16)}...)`);
  }

  const binDir = destDir || path.join(os.homedir(), ".droidpath", "bin");
  mkdirSync(binDir, { recursive: true });
  const exeName = platform === "win32" ? "droidpath-mcp.exe" : "droidpath-mcp";
  const dest = path.join(binDir, exeName);
  const tmpDest = `${dest}.${process.pid}.${Date.now()}.tmp`;
  try {
    writeFileSync(tmpDest, buf);
    if (platform !== "win32") {
      chmodSync(tmpDest, 0o755);
    }
    renameSync(tmpDest, dest);
  } catch (err) {
    try {
      rmSync(tmpDest, { force: true });
    } catch {
      // Ignore cleanup failure
    }
    throw err;
  }
  return dest;
}
