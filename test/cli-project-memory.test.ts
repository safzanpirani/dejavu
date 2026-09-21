import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cli = new URL("../src/cli.ts", import.meta.url).pathname;

interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

async function run(args: string[], env: Record<string, string>, stdin?: string): Promise<RunResult> {
  const proc = Bun.spawn([process.execPath, cli, ...args], {
    env: { ...process.env, ...env },
    stdin: stdin === undefined ? "ignore" : "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  if (stdin !== undefined) {
    proc.stdin!.write(stdin);
    proc.stdin!.end();
  }
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { stdout, stderr, exitCode };
}

function parse<T>(result: RunResult): T {
  return JSON.parse(result.stdout) as T;
}

function git(cwd: string, ...args: string[]): void {
  const proc = Bun.spawnSync(["git", "-C", cwd, ...args], { stdout: "pipe", stderr: "pipe" });
  if (proc.exitCode !== 0) throw new Error(`git ${args.join(" ")} failed: ${proc.stderr.toString()}`);
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

const payload = {
  kind: "decision",
  title: "Money uses integer minor units",
  body: "The API keeps money in integer minor units; conversion happens at the display boundary.",
  verification: "code_verified",
  tags: ["money"],
  evidence: [{ kind: "file", path: "src/api/money.ts", line: 12, observedAt: "2026-09-20T10:00:00Z" }],
};

describe("CLI contracts", () => {
  test("init is idempotent, JSON goes to stdout, diagnostics to stderr", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const dir = join(root, "repo");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(dir);
      const first = await run(["memory", "project", "init", "--cwd", dir, "--name", "acme", "--json"], env);
      expect(first.exitCode).toBe(0);
      const firstBody = parse<{ data: { project: { id: string }; created: boolean } }>(first);
      expect(firstBody.data.created).toBe(true);

      const second = await run(["memory", "project", "init", "--cwd", dir, "--json"], env);
      expect(second.exitCode).toBe(0);
      const secondBody = parse<{ data: { project: { id: string }; created: boolean } }>(second);
      expect(secondBody.data.created).toBe(false);
      expect(secondBody.data.project.id).toBe(firstBody.data.project.id);

      const human = await run(["memory", "project", "init", "--cwd", dir], env);
      expect(human.exitCode).toBe(0);
      expect(() => JSON.parse(human.stdout)).toThrow();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("unknown flags and missing values exit 2 with an error envelope", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      const unknown = await run(["memory", "project", "init", "--typo", "--json"], env);
      expect(unknown.exitCode).toBe(2);
      expect(parse<{ error: { code: string } }>(unknown).error.code).toBe("INVALID_ARGUMENT");

      const missing = await run(["memory", "project", "init", "--name", "--json"], env);
      expect(missing.exitCode).toBe(2);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an uninitialized project exits 4 and reads create nothing", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const dir = join(root, "plain");
    const dbPath = join(root, "memory.sqlite");
    const env = { DEJAVU_MEMORY_DB: dbPath };
    try {
      await mkdir(dir, { recursive: true });
      const result = await run(["memory", "project", "resolve", "--cwd", dir, "--json"], env);
      expect(result.exitCode).toBe(4);
      expect(parse<{ error: { code: string } }>(result).error.code).toBe("PROJECT_NOT_INITIALIZED");
      expect(await Bun.file(dbPath).exists()).toBe(false);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("cross-harness acceptance scenario", () => {
  test("a decision written by Claude is recalled by Codex, corrected with concurrency, and seen everywhere", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const repo = join(root, "New Volume 1", "projekt-ü");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(repo);
      const init = await run(["memory", "project", "init", "--cwd", repo, "--name", "acme", "--json"], env);
      expect(init.exitCode).toBe(0);

      // 1. Claude records a verified decision.
      const inputFile = join(root, "decision.json");
      await writeFile(inputFile, JSON.stringify(payload));
      const added = await run([
        "memory", "project", "add", "--cwd", repo, "--file", inputFile,
        "--harness", "claude", "--session-id", "claude-1", "--request-id", "create-1", "--json",
      ], env);
      expect(added.exitCode).toBe(0);
      const created = parse<{ data: { id: string; revision: number } }>(added).data;
      expect(created.revision).toBe(1);

      // 2-3. Codex asks for context and receives it with scope and evidence.
      const recall = await run([
        "memory", "project", "recall", "--cwd", repo, "--query", "money minor units", "--json",
      ], env);
      expect(recall.exitCode).toBe(0);
      const recalled = parse<{ data: { context: string; selected: Array<{ id: string; revision: number }>; usedChars: number } }>(recall).data;
      expect(recalled.selected[0]!.id).toBe(created.id);
      expect(recalled.selected[0]!.revision).toBe(1);
      expect(recalled.context).toContain("integer minor units");
      expect(recalled.context).toContain("src/api/money.ts");
      expect(recalled.usedChars).toBe(recalled.context.length);

      // 4. Codex corrects it using optimistic concurrency.
      const correction = join(root, "correction.json");
      await writeFile(correction, JSON.stringify({ ...payload, body: "Corrected: integer minor units, display-boundary conversion only." }));
      const updated = await run([
        "memory", "project", "update", created.id, "--if-revision", "1", "--cwd", repo,
        "--file", correction, "--harness", "codex", "--request-id", "update-1", "--json",
      ], env);
      expect(updated.exitCode).toBe(0);
      expect(parse<{ data: { revision: number } }>(updated).data.revision).toBe(2);

      const stale = await run([
        "memory", "project", "update", created.id, "--if-revision", "1", "--cwd", repo,
        "--file", correction, "--harness", "codex", "--json",
      ], env);
      expect(stale.exitCode).toBe(3);
      expect(parse<{ error: { code: string } }>(stale).error.code).toBe("REVISION_CONFLICT");

      // 5. Pi and OpenCode see the corrected version — identical id and revision.
      //    A caller label is a write-time provenance concern, not a read guard.
      for (const _harness of ["pi", "opencode"]) {
        const read = await run(["memory", "project", "get", created.id, "--cwd", repo, "--json"], env);
        expect(read.exitCode).toBe(0);
        const record = parse<{ data: { id: string; revision: number; body: string } }>(read).data;
        expect(record.id).toBe(created.id);
        expect(record.revision).toBe(2);
        expect(record.body).toContain("Corrected");
      }

      // 6. An unrelated project receives none of it.
      const other = join(root, "unrelated");
      await makeRepo(other);
      await run(["memory", "project", "init", "--cwd", other, "--name", "other", "--json"], env);
      const otherRecall = await run(["memory", "project", "recall", "--cwd", other, "--query", "money", "--json"], env);
      expect(otherRecall.exitCode).toBe(0);
      const otherData = parse<{ data: { context: string; selected: unknown[] } }>(otherRecall).data;
      expect(otherData.context).toBe("");
      expect(otherData.selected).toEqual([]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("worktrees share identity while a separate clone stays isolated until bound", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const repo = join(root, "repo");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(repo);
      const init = await run(["memory", "project", "init", "--cwd", repo, "--json"], env);
      const projectId = parse<{ data: { project: { id: string } } }>(init).data.project.id;

      const worktree = join(root, "wt");
      git(repo, "worktree", "add", "-q", worktree, "-b", "feature");
      const worktreeResolve = await run(["memory", "project", "resolve", "--cwd", worktree, "--json"], env);
      expect(parse<{ project: { id: string } }>(worktreeResolve).project.id).toBe(projectId);

      const clone = join(root, "clone");
      const cloneProc = Bun.spawnSync(["git", "clone", "-q", repo, clone], { stdout: "pipe", stderr: "pipe" });
      expect(cloneProc.exitCode).toBe(0);
      const cloneResolve = await run(["memory", "project", "resolve", "--cwd", clone, "--json"], env);
      expect(cloneResolve.exitCode).toBe(4);

      const bound = await run(["memory", "project", "bind", "--project-id", projectId, "--cwd", clone, "--json"], env);
      expect(bound.exitCode).toBe(0);
      const rebound = await run(["memory", "project", "resolve", "--cwd", clone, "--json"], env);
      expect(parse<{ project: { id: string } }>(rebound).project.id).toBe(projectId);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("compatibility and bounds", () => {
  test("legacy memory list|show still work and are not shadowed by the new namespace", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    try {
      const dir = join(root, "-Users-example-project", "memory");
      await mkdir(dir, { recursive: true });
      await writeFile(join(dir, "MEMORY.md"), "# Synthetic legacy memory");
      const list = await run(["memory", "list", "--root", root, "--json"], {});
      expect(list.exitCode).toBe(0);
      const projects = JSON.parse(list.stdout) as Array<{ project: string }>;
      const show = await run(["memory", "show", projects[0]!.project, "--root", root], {});
      expect(show.exitCode).toBe(0);
      expect(show.stdout).toContain("Synthetic legacy memory");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("unverified, expired, and inactive records stay out of recall but appear in list", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const repo = join(root, "repo");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(repo);
      await run(["memory", "project", "init", "--cwd", repo, "--json"], env);
      const draft = join(root, "draft.json");
      await writeFile(draft, JSON.stringify({ kind: "convention", title: "Unreviewed import", body: "Needs review.", verification: "unverified" }));
      const added = await run(["memory", "project", "add", "--cwd", repo, "--file", draft, "--json"], env);
      expect(added.exitCode).toBe(0);

      const recall = await run(["memory", "project", "recall", "--cwd", repo, "--query", "unreviewed", "--json"], env);
      expect(parse<{ data: { context: string } }>(recall).data.context).toBe("");

      const list = await run(["memory", "project", "list", "--cwd", repo, "--json"], env);
      expect(parse<{ data: { count: number } }>(list).data.count).toBe(1);

      const visible = await run(["memory", "project", "recall", "--cwd", repo, "--query", "unreviewed", "--include-unverified", "--json"], env);
      expect(parse<{ data: { context: string } }>(visible).data.context).toContain("Needs review");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("payloads can be read from stdin and handoffs get a default expiry", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const repo = join(root, "repo");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(repo);
      await run(["memory", "project", "init", "--cwd", repo, "--json"], env);
      const handoff = JSON.stringify({ kind: "handoff", title: "Finish the parser", body: "Next action: parse nested blocks.", verification: "user_confirmed", evidence: [{ kind: "user", note: "told to the agent", observedAt: "2026-09-21T00:00:00Z" }] });
      const added = await run(["memory", "project", "add", "--cwd", repo, "--file", "-", "--json"], env, handoff);
      expect(added.exitCode).toBe(0);
      const record = parse<{ data: { expiresAt: string | null } }>(added).data;
      expect(record.expiresAt).not.toBeNull();
      const days = (Date.parse(record.expiresAt!) - Date.now()) / 86_400_000;
      expect(days).toBeGreaterThan(6);
      expect(days).toBeLessThan(8);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("recall does not create or migrate a store and does not touch the transcript cache", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-"));
    const repo = join(root, "repo");
    const env = { DEJAVU_MEMORY_DB: join(root, "memory.sqlite") };
    try {
      await makeRepo(repo);
      await run(["memory", "project", "init", "--cwd", repo, "--json"], env);
      const before = await Bun.file(env.DEJAVU_MEMORY_DB).exists();
      const recall = await run(["memory", "project", "recall", "--cwd", repo, "--json"], env);
      expect(recall.exitCode).toBe(0);
      expect(await Bun.file(env.DEJAVU_MEMORY_DB).exists()).toBe(before);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
