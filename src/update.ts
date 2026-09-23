import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import packageJson from "../package.json" with { type: "json" };

export const VERSION: string = packageJson.version;
export const UPDATE_REPOSITORY = "safzanpirani/dejavu";
const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;
const CHECK_TIMEOUT_MS = 2_000;
export const DISABLE_CHECK_ENV = "DEJAVU_NO_UPDATE_CHECK";

export interface UpdateCheck {
  checkedAt: number;
  latest: string;
  current: string;
}

export interface UpdateDeps {
  fetch?: typeof fetch;
  now?: () => number;
  checkPath?: string;
  executable?: string;
  compiled?: boolean;
  platform?: NodeJS.Platform;
  arch?: string;
  log?: (line: string) => void;
  runPackageManager?: (command: string, args: string[]) => void;
}

export function parseVersion(value: string): [number, number, number] | null {
  const core = value.trim().replace(/^v/, "").split(/[-+]/)[0]!;
  const parts = core.split(".");
  if (parts.length === 0 || parts.length > 3 || parts.some((part) => !/^\d+$/.test(part))) return null;
  const numbers = parts.map(Number);
  return [numbers[0] ?? 0, numbers[1] ?? 0, numbers[2] ?? 0];
}

/** Orders two dotted versions; unparsable values sort lowest. */
export function compareVersions(a: string, b: string): number {
  const left = parseVersion(a);
  const right = parseVersion(b);
  if (!left || !right) return left ? 1 : right ? -1 : 0;
  for (let index = 0; index < 3; index++) {
    if (left[index] !== right[index]) return left[index]! < right[index]! ? -1 : 1;
  }
  return 0;
}

export function versionFromReleaseUrl(location: string): string {
  const marker = "/releases/tag/";
  const index = location.lastIndexOf(marker);
  // GitHub redirects /releases/latest to /releases while a repository has no release.
  if (/\/releases\/?$/.test(location)) throw new Error(`${UPDATE_REPOSITORY} has no published release yet`);
  if (index < 0) throw new Error(`unexpected release location ${location}`);
  const tag = decodeURIComponent(location.slice(index + marker.length)).replace(/^v/, "");
  if (!parseVersion(tag)) throw new Error(`unexpected release tag ${tag}`);
  return tag;
}

/** Asset names match the release workflow: dejavu-<platform>-<arch>[.exe]. */
export function releaseAssetName(platform: NodeJS.Platform = process.platform, arch: string = process.arch): string {
  const os = platform === "win32" ? "windows" : platform;
  return `dejavu-${os}-${arch}${platform === "win32" ? ".exe" : ""}`;
}

export function checksumFor(checksums: string, asset: string): string | undefined {
  for (const line of checksums.split("\n")) {
    const fields = line.trim().split(/\s+/);
    if (fields.length === 2 && fields[1]!.replace(/^\*/, "") === asset) return fields[0]!.toLowerCase();
  }
  return undefined;
}

function releaseDownloadUrl(tag: string, asset: string): string {
  return `https://github.com/${UPDATE_REPOSITORY}/releases/download/${tag}/${asset}`;
}

/**
 * Reads the newest release tag from the redirect on /releases/latest, which is not rate-limited
 * the way the unauthenticated API is.
 */
export async function fetchLatestVersion(deps: UpdateDeps = {}, signal?: AbortSignal): Promise<string> {
  const url = `https://github.com/${UPDATE_REPOSITORY}/releases/latest`;
  const response = await (deps.fetch ?? fetch)(url, { method: "HEAD", redirect: "manual", signal, headers: { "User-Agent": `dejavu/${VERSION}` } });
  const location = response.headers.get("location");
  if (!location) throw new Error(`${url} returned ${response.status} without a release redirect`);
  return versionFromReleaseUrl(location);
}

async function fetchBytes(url: string, deps: UpdateDeps): Promise<Uint8Array> {
  const response = await (deps.fetch ?? fetch)(url, { headers: { "User-Agent": `dejavu/${VERSION}` } });
  if (!response.ok) throw new Error(`${url} returned ${response.status}`);
  return new Uint8Array(await response.arrayBuffer());
}

export function defaultCheckPath(): string {
  const stateRoot = process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state");
  return join(stateRoot, "dejavu", "update-check.json");
}

export async function readUpdateCheck(path = defaultCheckPath()): Promise<UpdateCheck | null> {
  try {
    const check = JSON.parse(await readFile(path, "utf8")) as UpdateCheck;
    return typeof check.checkedAt === "number" && typeof check.latest === "string" ? check : null;
  } catch {
    return null;
  }
}

async function writeUpdateCheck(check: UpdateCheck, path = defaultCheckPath()): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, JSON.stringify(check));
}

/** A compiled binary runs from Bun's embedded filesystem; a source checkout runs src/cli.ts. */
function isCompiled(): boolean {
  return Bun.main.startsWith("/$bunfs/") || /^[A-Z]:[\\/]~BUN[\\/]/i.test(Bun.main);
}

/**
 * The npm package keeps the binary in native/ beside its package.json, so an npm or Bun global
 * install is updated through its package manager rather than by replacing the binary in place.
 */
export async function packageManagerFor(executable: string): Promise<"npm" | "bun" | null> {
  const root = dirname(dirname(executable));
  try {
    const manifest = JSON.parse(await readFile(join(root, "package.json"), "utf8")) as { name?: string };
    if (manifest.name !== packageJson.name || basename(dirname(executable)) !== "native") return null;
  } catch {
    return null;
  }
  return root.replaceAll("\\", "/").includes("/.bun/") ? "bun" : "npm";
}

function runPackageManager(command: string, args: string[]): void {
  const result = spawnSync(command, args, { stdio: "inherit" });
  if (result.error) throw new Error(`${command} is not on PATH; run \`${command} ${args.join(" ")}\``);
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} exited with ${result.status}`);
}

export interface SelfUpdateResult {
  current: string;
  latest: string;
  updated: boolean;
  path?: string;
}

/** Installs the newest release over this binary after checking it against the release checksums. */
export async function selfUpdate(options: { checkOnly?: boolean } = {}, deps: UpdateDeps = {}): Promise<SelfUpdateResult> {
  const log = deps.log ?? ((line: string) => console.error(line));
  const latest = await fetchLatestVersion(deps);
  await writeUpdateCheck({ checkedAt: (deps.now ?? Date.now)(), latest, current: VERSION }, deps.checkPath).catch(() => {});
  if (compareVersions(latest, VERSION) <= 0 || options.checkOnly) return { current: VERSION, latest, updated: false };
  if (!(deps.compiled ?? isCompiled())) {
    throw new Error(`dejavu ${latest} is available, but this dejavu runs from a source checkout; run \`git pull\` there instead`);
  }
  const executable = await realpath(deps.executable ?? process.execPath);
  const manager = await packageManagerFor(executable);
  if (manager) {
    const args = manager === "bun" ? ["add", "-g", `${packageJson.name}@${latest}`] : ["install", "-g", `${packageJson.name}@${latest}`];
    log(`updating through ${manager}: ${manager} ${args.join(" ")}`);
    (deps.runPackageManager ?? runPackageManager)(manager, args);
    return { current: VERSION, latest, updated: true, path: executable };
  }
  const tag = `v${latest}`;
  const asset = releaseAssetName(deps.platform, deps.arch);
  const expected = checksumFor(new TextDecoder().decode(await fetchBytes(releaseDownloadUrl(tag, "checksums.txt"), deps)), asset);
  if (!expected) throw new Error(`release ${tag} has no prebuilt binary named ${asset}`);
  log(`downloading ${asset} ${tag}`);
  const binary = await fetchBytes(releaseDownloadUrl(tag, asset), deps);
  const actual = createHash("sha256").update(binary).digest("hex");
  if (actual !== expected) throw new Error(`${asset} checksum mismatch: expected ${expected}, got ${actual}`);
  await swapExecutable(executable, binary, deps.platform ?? process.platform);
  return { current: VERSION, latest, updated: true, path: executable };
}

async function swapExecutable(executable: string, binary: Uint8Array, platform: NodeJS.Platform): Promise<void> {
  const next = join(dirname(executable), `.dejavu-update-${process.pid}`);
  try {
    await writeFile(next, binary);
    await chmod(next, 0o755);
    // Windows cannot overwrite a running executable, but it can rename it aside first.
    if (platform === "win32") {
      await rm(`${executable}.old`, { force: true });
      await rename(executable, `${executable}.old`);
    }
    await rename(next, executable);
  } catch (error) {
    await rm(next, { force: true });
    throw error;
  }
}

/**
 * Refreshes the cached release lookup at most once a day, then returns a newer release if the
 * cache knows one. A failed lookup keeps the previous answer and still counts toward the interval.
 */
export async function availableUpdate(deps: UpdateDeps = {}): Promise<string | null> {
  if (process.env[DISABLE_CHECK_ENV] === "1") return null;
  const now = (deps.now ?? Date.now)();
  let check = await readUpdateCheck(deps.checkPath);
  const age = check ? now - check.checkedAt : Infinity;
  if (!check || check.current !== VERSION || age < 0 || age > CHECK_INTERVAL_MS) {
    let latest = check?.latest ?? "";
    try { latest = await fetchLatestVersion(deps, AbortSignal.timeout(CHECK_TIMEOUT_MS)); }
    catch { /* Keep the last known release through an outage. */ }
    check = { checkedAt: now, latest, current: VERSION };
    await writeUpdateCheck(check, deps.checkPath).catch(() => {});
  }
  return check.latest && compareVersions(check.latest, VERSION) > 0 ? check.latest : null;
}
