/**
 * Project identity: canonical paths, Git resolution, and boundary selection.
 *
 * Identity is never derived from a harness display slug, a basename, a remote
 * URL, or a branch. A project is an opaque UUID bound to canonical local roots,
 * so two checkouts with the same directory name stay distinct and every
 * worktree of one repository shares a single identity.
 */

import { realpath } from "node:fs/promises";
import { sep } from "node:path";
import { ProjectMemoryError, type ProjectBinding } from "./project-memory-types.ts";

export type BoundaryKind = "git_common_dir" | "folder";

export interface Boundary {
  kind: BoundaryKind;
  /** Canonical path that a binding is keyed by. */
  canonicalPath: string;
  /** Git checkout root, or the folder itself. */
  root: string;
  commonDir?: string;
  branch?: string;
  worktrees?: string[];
}

export interface GitProbe {
  /** Whether the directory is inside a Git working tree. */
  isRepo: boolean;
  checkoutRoot?: string;
  commonDir?: string;
  branch?: string;
  worktrees?: string[];
  /** Populated when Git itself could not answer (missing binary, permissions). */
  unavailable?: string;
  /** Populated when Git answered with a non-repository error. */
  notARepository?: boolean;
}

export interface IdentityDeps {
  realpath?: (path: string) => Promise<string>;
  git?: (cwd: string) => Promise<GitProbe>;
  /** Folder bindings only; Git bindings are keyed by common dir. */
  listFolderBindings?: () => Promise<ProjectBinding[]>;
}

const defaultRealpath = (path: string) => realpath(path);

function toPosix(path: string): string {
  return sep === "/" ? path : path.split(sep).join("/");
}

/** Strip trailing separators, keeping the filesystem root intact. */
export function normalizeCanonical(path: string): string {
  const posix = toPosix(path);
  if (posix === "/") return posix;
  return posix.replace(/\/+$/, "");
}

/**
 * Component-wise containment. `/a/b` contains `/a/b/c` but not `/a/bc`.
 * Never use raw string prefix checks for identity or authorization.
 */
export function isPathWithin(parent: string, child: string): boolean {
  const p = normalizeCanonical(parent);
  const c = normalizeCanonical(child);
  if (p === c) return true;
  return c.startsWith(p === "/" ? "/" : `${p}/`);
}

/** Canonicalize with a clear failure instead of a raw ENOENT. */
export async function canonicalize(path: string, deps: IdentityDeps = {}): Promise<string> {
  try {
    return normalizeCanonical(await (deps.realpath ?? defaultRealpath)(path));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new ProjectMemoryError("CWD_NOT_FOUND", `cannot resolve path '${path}': ${message}`, 2);
  }
}

/**
 * Normalize a repository-relative prefix for storage.
 *
 * Rejects absolute paths, drive letters, and any `..` segment. Exact case is
 * preserved: V1 compares prefixes case-sensitively and this is documented.
 */
export function normalizePathPrefix(prefix: string): string {
  const trimmed = prefix.trim();
  if (!trimmed) throw new ProjectMemoryError("INVALID_ARGUMENT", "path prefix must not be empty", 2);
  const posix = toPosix(trimmed).replace(/^\.\//, "").replace(/\/+$/, "");
  if (posix.startsWith("/") || /^[A-Za-z]:/.test(posix)) {
    throw new ProjectMemoryError("INVALID_ARGUMENT", `path prefix must be repository-relative: '${prefix}'`, 2);
  }
  const segments = posix.split("/");
  if (segments.some((segment) => segment === ".." || segment === "")) {
    throw new ProjectMemoryError("INVALID_ARGUMENT", `path prefix must not contain '..' or empty segments: '${prefix}'`, 2);
  }
  return posix;
}

/** Does a stored prefix apply to this repository-relative path? */
export function prefixApplies(prefix: string | undefined, relativePath: string | undefined): boolean {
  if (prefix === undefined) return true;
  if (relativePath === undefined) return false;
  const normalized = normalizeCanonical(relativePath);
  return normalized === prefix || normalized.startsWith(`${prefix}/`);
}

interface RunResult {
  code: number;
  stdout: string;
  stderr: string;
  spawnFailed?: string;
}

async function runGit(args: string[], cwd: string): Promise<RunResult> {
  let proc: ReturnType<typeof Bun.spawn>;
  try {
    proc = Bun.spawn(["git", "-C", cwd, ...args], { stdout: "pipe", stderr: "pipe", stdin: "ignore" });
  } catch (error) {
    return { code: -1, stdout: "", stderr: "", spawnFailed: error instanceof Error ? error.message : String(error) };
  }
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout as ReadableStream).text(),
    new Response(proc.stderr as ReadableStream).text(),
    proc.exited,
  ]);
  return { code, stdout, stderr };
}

const NOT_A_REPO = /not a git repository|not a working tree/i;

/**
 * Probe Git with argument arrays only — a path is never shell-interpolated.
 *
 * Distinguishes "not a repository" from an unavailable Git (missing binary,
 * permissions) and from a malformed repository, so callers can fall back to an
 * explicit folder binding instead of inventing a competing identity.
 */
export async function probeGit(cwd: string): Promise<GitProbe> {
  const top = await runGit(["rev-parse", "--path-format=absolute", "--show-toplevel"], cwd);
  if (top.spawnFailed) return { isRepo: false, unavailable: top.spawnFailed };

  let checkoutRoot: string | undefined;
  if (top.code === 0) {
    checkoutRoot = top.stdout.trim();
  } else if (NOT_A_REPO.test(top.stderr)) {
    return { isRepo: false, notARepository: true };
  } else if (/unknown option|usage:/i.test(top.stderr)) {
    // Older Git without --path-format: resolve the relative answer manually.
    const fallback = await runGit(["rev-parse", "--show-toplevel"], cwd);
    if (fallback.spawnFailed) return { isRepo: false, unavailable: fallback.spawnFailed };
    if (fallback.code !== 0) {
      return NOT_A_REPO.test(fallback.stderr)
        ? { isRepo: false, notARepository: true }
        : { isRepo: false, unavailable: fallback.stderr.trim() || `git exited ${fallback.code}` };
    }
    checkoutRoot = fallback.stdout.trim();
  } else {
    return { isRepo: false, unavailable: top.stderr.trim() || `git exited ${top.code}` };
  }
  if (!checkoutRoot) return { isRepo: false, unavailable: "git returned an empty checkout root" };

  let commonDir: string | undefined;
  const common = await runGit(["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd);
  if (common.code === 0) commonDir = common.stdout.trim();
  else {
    const fallback = await runGit(["rev-parse", "--git-common-dir"], cwd);
    if (fallback.code === 0) commonDir = fallback.stdout.trim();
  }
  if (!commonDir) return { isRepo: false, unavailable: "git returned no common directory" };
  // A relative answer is documented as relative to the working directory.
  if (!commonDir.startsWith("/")) commonDir = `${checkoutRoot}/${commonDir}`;

  const branchResult = await runGit(["symbolic-ref", "--quiet", "--short", "HEAD"], cwd);
  const branch = branchResult.code === 0 ? branchResult.stdout.trim() || undefined : undefined;

  const worktreeResult = await runGit(["worktree", "list", "--porcelain"], cwd);
  const worktrees = worktreeResult.code === 0
    ? worktreeResult.stdout.split("\n").filter((line) => line.startsWith("worktree ")).map((line) => normalizeCanonical(line.slice("worktree ".length).trim()))
    : undefined;

  return {
    isRepo: true,
    checkoutRoot: normalizeCanonical(checkoutRoot),
    commonDir: normalizeCanonical(commonDir),
    branch,
    worktrees,
  };
}

/**
 * Choose the nearest project boundary containing `cwd`.
 *
 * The nearest root wins, which implements both documented rules at once: a
 * registered nested folder outranks an enclosing Git checkout, and a nested Git
 * repository is its own boundary rather than inheriting an outer registration.
 */
export function selectBoundary(
  cwd: string,
  git: GitProbe,
  folderBindings: ProjectBinding[],
): Boundary | undefined {
  const candidates: Boundary[] = [];
  if (git.isRepo && git.checkoutRoot && git.commonDir) {
    candidates.push({
      kind: "git_common_dir",
      canonicalPath: git.commonDir,
      root: git.checkoutRoot,
      commonDir: git.commonDir,
      branch: git.branch,
      worktrees: git.worktrees,
    });
  }
  for (const binding of folderBindings) {
    if (isPathWithin(binding.canonicalPath, cwd)) {
      candidates.push({ kind: "folder", canonicalPath: binding.canonicalPath, root: binding.canonicalPath, branch: git.branch });
    }
  }
  if (candidates.length === 0) return undefined;
  candidates.sort((a, b) => {
    const depth = normalizeCanonical(b.root).length - normalizeCanonical(a.root).length;
    if (depth !== 0) return depth;
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    return a.canonicalPath.localeCompare(b.canonicalPath);
  });
  const nearest = candidates[0]!;
  // Two distinct boundaries at the same depth would be ambiguous; fail closed.
  const conflicting = candidates.find((candidate) =>
    candidate !== nearest
    && normalizeCanonical(candidate.root) === normalizeCanonical(nearest.root)
    && (candidate.kind !== nearest.kind || candidate.canonicalPath !== nearest.canonicalPath));
  if (conflicting) {
    throw new ProjectMemoryError(
      "AMBIGUOUS_PROJECT",
      `ambiguous project boundary at '${nearest.root}': ${nearest.kind} and ${conflicting.kind} both claim it`,
      1,
    );
  }
  return nearest;
}

/** Repository-relative POSIX path of `cwd` beneath `root`, when inside it. */
export function relativeToRoot(root: string, cwd: string): string | undefined {
  const r = normalizeCanonical(root);
  const c = normalizeCanonical(cwd);
  if (r === c) return undefined;
  if (!isPathWithin(r, c)) return undefined;
  return c.slice(r.length + 1);
}
