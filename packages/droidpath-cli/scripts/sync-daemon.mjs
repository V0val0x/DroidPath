import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const cliRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(cliRoot, "../..");
const srcJar = path.join(repoRoot, "android-daemon", "build", "dist", "android-daemon.jar");
const dstDir = path.join(cliRoot, "assets");
const dstJar = path.join(dstDir, "android-daemon.jar");

if (!existsSync(srcJar)) {
  console.error(`Source daemon JAR not found at ${srcJar}. Run ./android-daemon/scripts/build-dex.sh first.`);
  process.exit(1);
}

mkdirSync(dstDir, { recursive: true });
copyFileSync(srcJar, dstJar);
console.log(`Synced android-daemon.jar -> ${dstJar}`);
