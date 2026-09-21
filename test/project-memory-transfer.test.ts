import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ProjectMemoryStore, resolveMemoryDbPath } from "../src/project-memory-store.ts";
import { ProjectMemoryService } from "../src/project-memory.ts";
import { buildSnapshot, importSnapshot, planClaudeImport, applyClaudeImport, renderMarkdown, writeExport } from "../src/project-memory-transfer.ts";
import type { MemoryInput, MemoryRecord, ProjectRecord } from "../src/project-memory-types.ts";

const at = "2026-01-01T00:00:00.000Z";
const project: ProjectRecord = { id: "aaaaaaaa-0000-4000-8000-000000000001", name: "transfer", createdAt: at, updatedAt: at };

function input(overrides: Partial<MemoryInput> = {}): MemoryInput {
  return {
    kind: "decision",
    title: "Use a single transactional database",
    body: "Chosen because concurrent harness sessions must not overwrite each other.",
    verification: "user_confirmed",
    evidence: [{ kind: "user", note: "confirmed", observedAt: at }],
    tags: ["storage"],
    pinned: false,
    ...overrides,
  };
}

async function fixture(): Promise<{
  root: string;
  path: string;
  store: ProjectMemoryStore;
  service: ProjectMemoryService;
  ctx: { projectId: string; cwd: string; root: string };
  cleanup: () => Promise<void>;
}> {
  const root = await mkdtemp(join(tmpdir(), "dejavu-transfer-"));
  const path = join(root, "memory.sqlite");
  const store = ProjectMemoryStore.open({ path });
  store.createProject(project);
  const service = new ProjectMemoryService({ store });
  return {
    root,
    path,
    store,
    service,
    ctx: { projectId: project.id, cwd: root, root },
    cleanup: async () => { store.close(); await rm(root, { recursive: true, force: true }); },
  };
}

describe("export", () => {
  test("portable export drops local provenance paths and marks the record", async () => {
    const f = await fixture();
    try {
      f.service.add(f.ctx, input({
        verification: "code_verified",
        evidence: [{ kind: "file", path: "/home/someone/secret/project/x.ts", observedAt: at }],
      }), { harness: "claude" });
      const snapshot = buildSnapshot(f.store, project.id);
      expect(snapshot.records[0]!.evidence).toEqual([]);
      expect(snapshot.records[0]!.provenanceReduced).toBe(true);
      expect(JSON.stringify(snapshot)).not.toContain("/home/someone");
    } finally {
      await f.cleanup();
    }
  });

  test("--include-local-metadata keeps provenance", async () => {
    const f = await fixture();
    try {
      f.service.add(f.ctx, input({
        verification: "code_verified",
        evidence: [{ kind: "file", path: "/home/someone/project/x.ts", observedAt: at }],
      }), { harness: "claude" });
      const snapshot = buildSnapshot(f.store, project.id, { includeLocalMetadata: true });
      expect(snapshot.records[0]!.evidence).toHaveLength(1);
    } finally {
      await f.cleanup();
    }
  });

  test("markdown export carries stable ids and metadata", async () => {
    const f = await fixture();
    try {
      const added = f.service.add(f.ctx, input(), { harness: "claude" });
      const markdown = renderMarkdown(buildSnapshot(f.store, project.id));
      expect(markdown).toContain(added.record.id);
      expect(markdown).toContain("- kind: decision");
      expect(markdown).toContain("- verification: user_confirmed");
    } finally {
      await f.cleanup();
    }
  });

  test("writeExport refuses nothing but writes atomically", async () => {
    const f = await fixture();
    try {
      const output = join(f.root, "out", "snapshot.json");
      await mkdir(join(f.root, "out"), { recursive: true });
      await writeExport(output, "{}");
      expect(await Bun.file(output).text()).toBe("{}");
      expect((await readdir(join(f.root, "out"))).filter((name) => name.includes(".tmp-"))).toEqual([]);
    } finally {
      await f.cleanup();
    }
  });
});

describe("import", () => {
  test("a dry run performs no writes", async () => {
    const source = await fixture();
    const target = await fixture();
    try {
      source.service.add(source.ctx, input(), { harness: "claude" });
      const raw = JSON.stringify(buildSnapshot(source.store, project.id));
      const before = target.store.memoryCount(target.ctx.projectId);
      const result = importSnapshot(target.store, raw, { projectId: target.ctx.projectId, dryRun: true });
      expect(result.diagnostics.join(" ")).toContain("dry run");
      expect(target.store.memoryCount(target.ctx.projectId)).toBe(before);
    } finally {
      await source.cleanup();
      await target.cleanup();
    }
  });

  test("restoring into the same project preserves ids and relationships", async () => {
    const f = await fixture();
    try {
      const original = f.service.add(f.ctx, input(), { harness: "claude" }).record;
      const replacement = f.service.supersede(f.ctx, original.id, 1, input({ title: "Replaced" }), { harness: "codex" }).record;
      const raw = JSON.stringify(buildSnapshot(f.store, project.id));

      const target = await fixture();
      try {
        const result = importSnapshot(target.store, raw, { projectId: target.ctx.projectId });
        expect(result.imported).toBe(2);
        expect(target.store.getMemory(target.ctx.projectId, original.id)).toBeDefined();
        expect(target.store.getMemory(target.ctx.projectId, replacement.id)).toBeDefined();
        expect(target.store.getMemory(target.ctx.projectId, original.id)!.supersededBy).toBe(replacement.id);
      } finally {
        await target.cleanup();
      }
    } finally {
      await f.cleanup();
    }
  });

  test("importing into another project remaps ids and supersession links", async () => {
    const source = await fixture();
    try {
      const original = source.service.add(source.ctx, input(), { harness: "claude" }).record;
      const replacement = source.service.supersede(source.ctx, original.id, 1, input({ title: "Replaced" }), { harness: "codex" }).record;
      const raw = JSON.stringify(buildSnapshot(source.store, project.id));

      const other = { id: "bbbbbbbb-0000-4000-8000-000000000002", name: "other", createdAt: at, updatedAt: at };
      const root = await mkdtemp(join(tmpdir(), "dejavu-transfer-"));
      const store = ProjectMemoryStore.open({ path: join(root, "memory.sqlite") });
      store.createProject(other);
      try {
        const result = importSnapshot(store, raw, { projectId: other.id });
        expect(result.imported).toBe(2);
        expect(store.getMemory(other.id, original.id)).toBeUndefined();
        const imported = store.allMemories(other.id);
        const remappedOriginal = imported.find((record) => record.title === original.title)!;
        const remappedReplacement = imported.find((record) => record.title === "Replaced")!;
        expect(remappedOriginal.id).not.toBe(original.id);
        expect(remappedOriginal.supersededBy).toBe(remappedReplacement.id);
      } finally {
        store.close();
        await rm(root, { recursive: true, force: true });
      }
    } finally {
      await source.cleanup();
    }
  });

  test("re-importing identical content is idempotent and differing content conflicts", async () => {
    const f = await fixture();
    try {
      const record = f.service.add(f.ctx, input(), { harness: "claude" }).record;
      const raw = JSON.stringify(buildSnapshot(f.store, project.id));
      const rerun = importSnapshot(f.store, raw, { projectId: project.id });
      expect(rerun.imported).toBe(0);
      expect(rerun.skipped).toBe(1);

      const snapshot = buildSnapshot(f.store, project.id);
      snapshot.records[0]!.body = "different content";
      expect(() => importSnapshot(f.store, JSON.stringify(snapshot), { projectId: project.id })).toThrow(/different content/);
      expect(f.store.getMemory(project.id, record.id)!.body).not.toBe("different content");
    } finally {
      await f.cleanup();
    }
  });

  test("a batch with a rejected record writes nothing at all", async () => {
    const f = await fixture();
    try {
      const snapshot = buildSnapshot(f.store, project.id);
      snapshot.records = [
        { ...buildSnapshot(f.store, project.id).records[0], id: "not-a-uuid", title: "Broken" } as never,
      ];
      const before = f.store.memoryCount(project.id);
      const result = importSnapshot(f.store, JSON.stringify(snapshot), { projectId: project.id, dryRun: true });
      expect(result.rejected.length).toBeGreaterThan(0);
      expect(f.store.memoryCount(project.id)).toBe(before);
    } finally {
      await f.cleanup();
    }
  });

  test("a malformed snapshot is rejected before any write", async () => {
    const f = await fixture();
    try {
      expect(() => importSnapshot(f.store, "{not json", { projectId: project.id })).toThrow(/not valid JSON/);
      expect(() => importSnapshot(f.store, JSON.stringify({ kind: "wrong" }), { projectId: project.id })).toThrow(/kind must be/);
      expect(f.store.memoryCount(project.id)).toBe(0);
    } finally {
      await f.cleanup();
    }
  });
});

describe("Claude migration", () => {
  async function claudeDir(root: string): Promise<string> {
    const dir = join(root, "claude-memory");
    await mkdir(dir, { recursive: true });
    await writeFile(join(dir, "deploy.md"), "# Production deploy\n\nRun the migrator before the app boots.\n");
    await writeFile(join(dir, "MEMORY.md"), "# Index\n\n- [deploy](./deploy.md)\n- [other](./other.md)\n");
    return dir;
  }

  test("dry-run plans imports, skips index files, and keeps them unverified", async () => {
    const f = await fixture();
    try {
      const dir = await claudeDir(f.root);
      const planned = await planClaudeImport(f.store, dir, project.id);
      expect(planned.plan.entries.map((entry) => [entry.title, entry.action])).toEqual([
        ["MEMORY.md", "skip-index"],
        ["deploy.md", "import"],
      ]);
      expect(planned.payloads[0]!.input.verification).toBe("unverified");
      expect(planned.payloads[0]!.input.title).toBe("Production deploy");
      expect(f.store.memoryCount(project.id)).toBe(0);
    } finally {
      await f.cleanup();
    }
  });

  test("re-running an unchanged import does not duplicate records", async () => {
    const f = await fixture();
    try {
      const dir = await claudeDir(f.root);
      const first = await planClaudeImport(f.store, dir, project.id);
      applyClaudeImport(f.store, f.service, first, f.ctx);
      expect(f.store.memoryCount(project.id)).toBe(1);

      const second = await planClaudeImport(f.store, dir, project.id);
      expect(second.plan.entries.find((entry) => entry.title === "deploy.md")!.action).toBe("skip-unchanged");
      expect(second.payloads).toHaveLength(0);
      expect(f.store.memoryCount(project.id)).toBe(1);
    } finally {
      await f.cleanup();
    }
  });

  test("changed source content is a review candidate, never an overwrite", async () => {
    const f = await fixture();
    try {
      const dir = await claudeDir(f.root);
      const first = await planClaudeImport(f.store, dir, project.id);
      const applied = applyClaudeImport(f.store, f.service, first, f.ctx);
      const record = applied.records[0]!;
      const revised = f.service.update(f.ctx, record.id, record.revision, input({ title: "Curated by an agent" }), { harness: "codex" });

      await writeFile(join(dir, "deploy.md"), "# Production deploy\n\nCompletely different instruction now.\n");
      const second = await planClaudeImport(f.store, dir, project.id);
      expect(second.plan.entries.find((entry) => entry.title === "deploy.md")!.action).toBe("review-changed");
      expect(second.payloads).toHaveLength(0);
      expect(f.store.getMemory(project.id, record.id)!.title).toBe("Curated by an agent");
      expect(revised.record.revision).toBe(2);
    } finally {
      await f.cleanup();
    }
  });
});

describe("recovery", () => {
  test("a backup restores into a separate database and keeps records", async () => {
    const f = await fixture();
    try {
      f.service.add(f.ctx, input(), { harness: "claude" });
      const backupPath = join(f.root, "backups", "memory-backup.sqlite");
      f.store.backup(backupPath);
      const restored = ProjectMemoryStore.open({ path: backupPath });
      try {
        expect(restored.integrity()).toBe("ok");
        expect(restored.memoryCount(project.id)).toBe(1);
        expect(restored.getProject(project.id)?.name).toBe("transfer");
      } finally {
        restored.close();
      }
    } finally {
      await f.cleanup();
    }
  });

  test("backup refuses to overwrite an existing file", async () => {
    const f = await fixture();
    try {
      const backupPath = join(f.root, "backup.sqlite");
      f.store.backup(backupPath);
      expect(() => f.store.backup(backupPath)).toThrow(/refusing to overwrite/);
    } finally {
      await f.cleanup();
    }
  });

  test("the memory database path never inherits DEJAVU_INDEX_PATH", () => {
    const previousIndex = process.env.DEJAVU_INDEX_PATH;
    const previousMemory = process.env.DEJAVU_MEMORY_DB;
    try {
      process.env.DEJAVU_INDEX_PATH = "/tmp/should-be-ignored.sqlite";
      delete process.env.DEJAVU_MEMORY_DB;
      const resolved = resolveMemoryDbPath();
      expect(resolved).not.toContain("should-be-ignored");
      expect(resolved).toContain("memory.sqlite");
      process.env.DEJAVU_MEMORY_DB = "/tmp/explicit-memory.sqlite";
      expect(resolveMemoryDbPath()).toBe("/tmp/explicit-memory.sqlite");
    } finally {
      if (previousIndex === undefined) delete process.env.DEJAVU_INDEX_PATH; else process.env.DEJAVU_INDEX_PATH = previousIndex;
      if (previousMemory === undefined) delete process.env.DEJAVU_MEMORY_DB; else process.env.DEJAVU_MEMORY_DB = previousMemory;
    }
  });
});
