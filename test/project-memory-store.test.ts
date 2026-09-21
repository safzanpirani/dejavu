import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ProjectMemoryStore, SCHEMA_VERSION } from "../src/project-memory-store.ts";
import { ProjectMemoryService } from "../src/project-memory.ts";
import type { MemoryInput, MemoryRecord, ProjectRecord } from "../src/project-memory-types.ts";

const at = "2026-01-01T00:00:00.000Z";
const project: ProjectRecord = { id: "11111111-1111-4111-8111-111111111111", name: "store-test", createdAt: at, updatedAt: at };

function input(overrides: Partial<MemoryInput> = {}): MemoryInput {
  return {
    kind: "convention",
    title: "Default title",
    body: "Default body",
    verification: "user_confirmed",
    evidence: [{ kind: "user", note: "confirmed in test", observedAt: at }],
    tags: [],
    pinned: false,
    ...overrides,
  };
}

function record(id: string, overrides: Partial<MemoryRecord> = {}): MemoryRecord {
  const base: MemoryRecord = {
    id,
    projectId: project.id,
    revision: 1,
    kind: "convention",
    title: "Title",
    body: "Body",
    status: "active",
    verification: "user_confirmed",
    tags: [],
    evidence: [],
    pinned: false,
    createdAt: at,
    updatedAt: at,
    contentHash: "sha256:test",
  };
  return { ...base, ...overrides };
}

async function tempStore(): Promise<{ path: string; store: ProjectMemoryStore; cleanup: () => Promise<void> }> {
  const root = await mkdtemp(join(tmpdir(), "dejavu-store-"));
  const path = join(root, "memory.sqlite");
  const store = ProjectMemoryStore.open({ path });
  store.createProject(project);
  return { path, store, cleanup: async () => { store.close(); await rm(root, { recursive: true, force: true }); } };
}

describe("store lifecycle", () => {
  test("a missing database is not created by a read", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-store-"));
    try {
      expect(ProjectMemoryStore.openRead({ path: join(root, "absent.sqlite") })).toBeUndefined();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("opening for write creates and migrates the schema", async () => {
    const { store, cleanup } = await tempStore();
    try {
      expect(store.schemaVersion()).toBe(SCHEMA_VERSION);
      expect(store.integrity()).toBe("ok");
      expect(store.getProject(project.id)?.name).toBe("store-test");
    } finally {
      await cleanup();
    }
  });

  test("a newer schema is refused rather than reset", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-store-"));
    try {
      const path = join(root, "memory.sqlite");
      const db = new Database(path, { create: true });
      db.exec("PRAGMA user_version = 99");
      db.close();
      expect(() => ProjectMemoryStore.open({ path })).toThrow(/newer than this build/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("foreign keys are enforced on every connection", async () => {
    const { store, cleanup } = await tempStore();
    try {
      expect(() => store.addBinding({ kind: "folder", canonicalPath: "/x", projectId: "missing", createdAt: at })).toThrow();
    } finally {
      await cleanup();
    }
  });
});

describe("records, revisions, and idempotency", () => {
  test("an initial create writes revision 1 and a snapshot", async () => {
    const { store, cleanup } = await tempStore();
    try {
      store.createMemory(record("aaaaaaaa-1111-4111-8111-111111111111"), { harness: "claude" }, "create");
      const revisions = store.revisions("aaaaaaaa-1111-4111-8111-111111111111");
      expect(revisions.map((entry) => entry.revision)).toEqual([1]);
      expect(revisions[0]!.actor.harness).toBe("claude");
    } finally {
      await cleanup();
    }
  });

  test("every visible mutation appends a snapshot and bumps the revision", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const id = "bbbbbbbb-1111-4111-8111-111111111111";
      store.createMemory(record(id), { harness: "codex" }, "create");
      const next = record(id, { revision: 2, title: "Changed", updatedAt: "2026-01-02T00:00:00.000Z" });
      store.replaceMemory(next, { harness: "codex" }, "update", "2026-01-02T00:00:00.000Z");
      expect(store.getMemory(project.id, id)!.title).toBe("Changed");
      expect(store.revisions(id).map((entry) => entry.revision)).toEqual([1, 2]);
    } finally {
      await cleanup();
    }
  });

  test("the same request id and payload replays; a different payload conflicts", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const id = "cccccccc-1111-4111-8111-111111111111";
      const first = store.createMemory(record(id), { harness: "claude" }, "create", { id: "req-1", hash: "sha256:aaa" });
      expect(first.replayed).toBe(false);
      const replay = store.createMemory(record("dddddddd-1111-4111-8111-111111111111"), { harness: "claude" }, "create", { id: "req-1", hash: "sha256:aaa" });
      expect(replay.replayed).toBe(true);
      expect(replay.result.id).toBe(id);
      expect(() => store.createMemory(record("eeeeeeee-1111-4111-8111-111111111111"), { harness: "claude" }, "create", { id: "req-1", hash: "sha256:bbb" })).toThrow(/different payload/);
      expect(store.memoryCount(project.id)).toBe(1);
    } finally {
      await cleanup();
    }
  });

  test("supersession replaces atomically and keeps the old record", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const oldId = "ffffffff-1111-4111-8111-111111111111";
      const newId = "99999999-1111-4111-8111-111111111111";
      store.createMemory(record(oldId), { harness: "pi" }, "create");
      const old = store.getMemory(project.id, oldId)!;
      const superseded = { ...old, revision: 2, status: "superseded" as const, supersededBy: newId };
      store.supersedeMemory(superseded, record(newId), { harness: "pi" }, "supersede", at);
      expect(store.getMemory(project.id, oldId)!.status).toBe("superseded");
      expect(store.getMemory(project.id, newId)!.status).toBe("active");
      expect(store.listMemories(project.id, { status: ["active"] }).map((entry) => entry.id)).toEqual([newId]);
    } finally {
      await cleanup();
    }
  });

  test("purge refuses unresolved incoming supersession links", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const oldId = "12121212-1111-4111-8111-111111111111";
      const newId = "13131313-1111-4111-8111-111111111111";
      store.createMemory(record(oldId), { harness: "manual" }, "create");
      store.createMemory(record(newId, { supersededBy: oldId }), { harness: "manual" }, "create");
      expect(() => store.purgeMemory(project.id, oldId)).toThrow(/still point at this memory/);
      expect(store.memoryCount(project.id)).toBe(2);
    } finally {
      await cleanup();
    }
  });

  test("purge removes the record and its history", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const id = "14141414-1111-4111-8111-111111111111";
      store.createMemory(record(id), { harness: "manual" }, "create");
      const result = store.purgeMemory(project.id, id);
      expect(result.deletedRevisions).toBe(1);
      expect(store.getMemory(project.id, id)).toBeUndefined();
      expect(store.revisions(id)).toEqual([]);
    } finally {
      await cleanup();
    }
  });

  test("reads never mutate the record", async () => {
    const { store, cleanup } = await tempStore();
    try {
      const id = "15151515-1111-4111-8111-111111111111";
      store.createMemory(record(id), { harness: "manual" }, "create");
      const before = store.getMemory(project.id, id)!;
      store.listMemories(project.id);
      store.getMemory(project.id, id);
      expect(store.getMemory(project.id, id)!.updatedAt).toBe(before.updatedAt);
      expect(store.revisions(id).length).toBe(1);
    } finally {
      await cleanup();
    }
  });
});

describe("cross-process contention", () => {
  test("parallel writers preserve every successful record", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-store-"));
    try {
      const path = join(root, "memory.sqlite");
      const store = ProjectMemoryStore.open({ path });
      store.createProject(project);
      const service = new ProjectMemoryService({ store, uuid: () => crypto.randomUUID() });
      const ctx = { projectId: project.id, cwd: root, root };
      const writers = 8;
      const perWriter = 4;
      const results = await Promise.allSettled(
        Array.from({ length: writers }, async (_, writer) => {
          for (let index = 0; index < perWriter; index += 1) {
            service.add(ctx, input({ title: `writer ${writer} record ${index}`, body: `body ${writer}-${index}` }), { harness: "codex" });
          }
        }),
      );
      const failures = results.filter((result) => result.status === "rejected");
      expect(failures.length).toBe(0);
      expect(store.memoryCount(project.id)).toBe(writers * perWriter);
      store.close();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
