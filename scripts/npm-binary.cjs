// Locates or fetches the dejavu binary for this platform. Shared by the npm launcher and the
// postinstall hook. Plain CommonJS so it runs under Node 18+ and Bun without a build step.
"use strict";

const { createHash } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const packageRoot = path.resolve(__dirname, "..");
const manifest = require(path.join(packageRoot, "package.json"));
const repository = "safzanpirani/dejavu";

/** Matches the release asset names: dejavu-<platform>-<arch>[.exe]. */
function assetName() {
  const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[process.platform];
  const arch = { x64: "x64", arm64: "arm64" }[process.arch];
  if (!platform || !arch || (platform === "windows" && arch !== "x64")) return undefined;
  return `dejavu-${platform}-${arch}${platform === "windows" ? ".exe" : ""}`;
}

function binaryPath() {
  if (process.env.DEJAVU_BINARY) return process.env.DEJAVU_BINARY;
  return path.join(packageRoot, "native", process.platform === "win32" ? "dejavu.exe" : "dejavu");
}

function pinnedChecksums() {
  try {
    return JSON.parse(fs.readFileSync(path.join(packageRoot, "checksums.json"), "utf8"));
  } catch {
    return {};
  }
}

/**
 * Returns the path of a usable binary, downloading the release binary pinned by checksums.json
 * when it is missing. Throws when no verified download is possible.
 */
async function ensureBinary(options = {}) {
  const log = options.log || (() => undefined);
  const destination = binaryPath();
  if (fs.existsSync(destination)) return destination;
  if (process.env.DEJAVU_BINARY) throw new Error(`DEJAVU_BINARY points at ${destination}, which does not exist`);
  const asset = assetName();
  if (!asset) throw new Error(`no prebuilt dejavu binary for ${process.platform}/${process.arch}`);
  const expected = pinnedChecksums()[asset];
  if (typeof expected !== "string" || !/^[0-9a-f]{64}$/i.test(expected)) throw new Error(`no pinned checksum for ${asset}`);
  const url = `https://github.com/${repository}/releases/download/v${manifest.version}/${asset}`;
  log(`dejavu: downloading ${url}`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`download failed: ${response.status} ${response.statusText}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== expected.toLowerCase()) throw new Error(`checksum mismatch for ${asset}: expected ${expected}, got ${actual}`);
  const temporary = `${destination}.${process.pid}.tmp`;
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  try {
    fs.writeFileSync(temporary, bytes, { mode: 0o755 });
    fs.renameSync(temporary, destination);
  } catch (error) {
    fs.rmSync(temporary, { force: true });
    throw error;
  }
  return destination;
}

module.exports = { assetName, binaryPath, ensureBinary, packageRoot };
