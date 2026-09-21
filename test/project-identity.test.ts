import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  canonicalize,
  isPathWithin,
  normalizePathPrefix,
  prefixApplies,
  probeGit,
  relativeToRoot,
  selectBoundary,
} from "../src/project-identity.ts";
import type { ProjectBinding } from "../src/project-memory-types.ts";

function git(cwd: string, ...args: string[]): string {
  const proc = Bun.spawnSync(["git", "-C", cwd, ...args], { stdout: "pipe", stderr: "pipe" });
  if (proc.exitCode !== 0) throw new Error(`git ${args.join(" ")} failed: ${proc.stderr.toString()}`);
  return proc.stdout.toString().trim();
}

async function makeRepo(dir: string): Promise<void> {
  await mkdir(dir, { recursive: true });
  git(dir, "init", "-q", "-b", "main");
  git(dir, "config", "user.email", "tests@example.invalid");
  git(dir, "config", "user.name", "tests");
  await writeFile(join(dir, "README.md"), "x\n");
  git(dir, "add", "-A");
  git(dir, "commit", "-qm", "init");
}

const folder = (path: string): ProjectBinding => ({
  kind: "folder",
  canonicalPath: path,
  projectId: "p",
  createdAt: "2026-01-01T00:00:00.000Z",
});

describe("path containment and prefixes", () => {
  test("component containment does not match a sibling with a shared prefix", () => {
    expect(isPathWithin("/a/b", "/a/b/c")).toBe(true);
    expect(isPathWithin("/a/b", "/a/b")).toBe(true);
    expect(isPathWithin("/a/b", "/a/bc")).toBe(false);
    expect(isPathWithin("/", "/a/b")).toBe(true);
  });

  test("repository-relative prefixes reject absolute, drive, and parent segments", () => {
    expect(normalizePathPrefix("packages/api")).toBe("packages/api");
    expect(normalizePathPrefix("./src//")).toBe("src");
    expect(() => normalizePathPrefix("/etc")).toThrow();
    expect(() => normalizePathPrefix("C:\\windows")).toThrow();
    expect(() => normalizePathPrefix("../secrets")).toThrow();
    expect(() => normalizePathPrefix("a/../b")).toThrow();
  });

  test("a stored prefix honours path segments", () => {
    expect(prefixApplies("packages/api", "packages/api/src/x.ts")).toBe(true);
    expect(prefixApplies("packages/api", "packages/api")).toBe(true);
    expect(prefixApplies("packages/api", "packages/api-client/x.ts")).toBe(false);
    expect(prefixApplies("packages/api", undefined)).toBe(false);
    expect(prefixApplies(undefined, "anything")).toBe(true);
  });

  test("relative paths are computed only below the root", () => {
    expect(relativeToRoot("/a/b", "/a/b/c/d")).toBe("c/d");
    expect(relativeToRoot("/a/b", "/a/b")).toBeUndefined();
    expect(relativeToRoot("/a/b", "/a/c")).toBeUndefined();
  });

  test("a missing path fails clearly instead of throwing ENOENT", async () => {
    await expect(canonicalize("/definitely/not/here")).rejects.toThrow(/cannot resolve path/);
  });
});

describe("git probe", () => {
  test("detects a repository, branch, and common directory", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const repo = join(root, "repo");
      await makeRepo(repo);
      const probe = await probeGit(repo);
      expect(probe.isRepo).toBe(true);
      expect(probe.checkoutRoot).toBe(repo);
      expect(probe.commonDir).toBe(join(repo, ".git"));
      expect(probe.branch).toBe("main");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("separates 'not a repository' from an unavailable git", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const plain = join(root, "plain");
      await mkdir(plain, { recursive: true });
      const probe = await probeGit(plain);
      expect(probe.isRepo).toBe(false);
      expect(probe.notARepository).toBe(true);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a linked worktree shares the main repository's common directory", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const repo = join(root, "repo");
      await makeRepo(repo);
      const worktree = join(root, "wt");
      git(repo, "worktree", "add", "-q", worktree, "-b", "feature");
      const main = await probeGit(repo);
      const linked = await probeGit(worktree);
      expect(linked.commonDir).toBe(main.commonDir);
      expect(linked.checkoutRoot).toBe(worktree);
      expect(linked.branch).toBe("feature");
      expect(linked.worktrees).toContain(repo);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("boundary selection", () => {
  test("same basename in different parents yields distinct boundaries", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const first = join(root, "one", "project");
      const second = join(root, "two", "project");
      await makeRepo(first);
      await makeRepo(second);
      const a = selectBoundary(first, await probeGit(first), [])!;
      const b = selectBoundary(second, await probeGit(second), [])!;
      expect(a.canonicalPath).not.toBe(b.canonicalPath);
      expect(a.root).not.toBe(b.root);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("paths with spaces and Unicode resolve", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const repo = join(root, "New Volume 1", "projekt-ü");
      await makeRepo(repo);
      const probe = await probeGit(repo);
      const boundary = selectBoundary(repo, probe, [])!;
      expect(boundary.root).toBe(repo);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a registered nested folder outranks an enclosing checkout", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const repo = join(root, "repo");
      await makeRepo(repo);
      const nested = join(repo, "packages", "api");
      await mkdir(nested, { recursive: true });
      const boundary = selectBoundary(nested, await probeGit(nested), [folder(nested)])!;
      expect(boundary.kind).toBe("folder");
      expect(boundary.root).toBe(nested);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a nested repository is its own boundary and does not inherit an outer registration", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const outer = join(root, "outer");
      await makeRepo(outer);
      const inner = join(outer, "vendor", "inner");
      await makeRepo(inner);
      const boundary = selectBoundary(inner, await probeGit(inner), [folder(outer)])!;
      expect(boundary.kind).toBe("git_common_dir");
      expect(boundary.root).toBe(inner);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("no boundary means the caller must initialize", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const plain = join(root, "plain");
      await mkdir(plain, { recursive: true });
      expect(selectBoundary(plain, await probeGit(plain), [])).toBeUndefined();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a symlinked working directory resolves to the real root", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-identity-"));
    try {
      const repo = join(root, "real");
      await makeRepo(repo);
      const link = join(root, "link");
      await symlink(repo, link);
      const canonical = await canonicalize(link);
      expect(canonical).toBe(repo);
      const boundary = selectBoundary(canonical, await probeGit(canonical), [])!;
      expect(boundary.root).toBe(repo);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
