import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { appendFile, mkdir, mkdtemp, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  refreshTranscriptIndex,
  searchTranscriptIndex,
  searchTranscriptIndexMatches,
  transcriptIndexStatus,
} from "../src/transcript-index.ts";
import { parseOpenCodeLocator } from "../src/opencode-store.ts";
import type { TranscriptStore } from "../src/transcript-types.ts";

function claudeLine(role: "user" | "assistant", text: string): string {
  return JSON.stringify({
    type: role,
    timestamp: "2026-09-02T10:00:00Z",
    cwd: "/work/dejavu",
    message: { role, content: [{ type: "text", text }] },
  });
}

describe("transcript index", () => {
  test("incrementally replaces changed files and removes deleted files", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-"));
    const storePath = join(root, "claude");
    const transcriptPath = join(storePath, "session.jsonl");
    const indexPath = join(root, "cache", "index.sqlite");
    const stores: TranscriptStore[] = [{ source: "claude", kind: "jsonl", path: storePath }];
    await mkdir(storePath, { recursive: true });
    await writeFile(transcriptPath, `${claudeLine("user", "Needle phrase appears twice: needle phrase")}`);

    try {
      const initial = await refreshTranscriptIndex(stores, indexPath);
      expect(initial).toMatchObject({ files: 1, messages: 1, indexed: 1, removed: 0 });
      expect(searchTranscriptIndex("needle phrase", stores, indexPath)).toEqual([{
        store: storePath, source: "claude", path: transcriptPath, count: 2,
      }]);
      expect(searchTranscriptIndexMatches("needle phrase", stores, indexPath, 10, 1)[0]).toMatchObject({
        store: storePath, source: "claude", path: transcriptPath, count: 2,
        date: "2026-09-02", project: "/work/dejavu",
        snippets: [{ role: "user" }],
      });

      const unchanged = await refreshTranscriptIndex(stores, indexPath);
      expect(unchanged.indexed).toBe(0);

      await writeFile(transcriptPath, `${claudeLine("assistant", "Replacement text only")}`);
      const changed = await refreshTranscriptIndex(stores, indexPath);
      expect(changed.indexed).toBe(1);
      expect(searchTranscriptIndex("needle phrase", stores, indexPath)).toEqual([]);
      expect(searchTranscriptIndex("replacement text", stores, indexPath)[0]?.count).toBe(1);

      await unlink(transcriptPath);
      const deleted = await refreshTranscriptIndex(stores, indexPath);
      expect(deleted).toMatchObject({ files: 0, messages: 0, removed: 1 });
      expect(await transcriptIndexStatus(indexPath)).toMatchObject({ exists: true, files: 0, messages: 0 });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("verifies exact literal text after trigram candidate lookup", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-literal-"));
    const storePath = join(root, "codex");
    const transcriptPath = join(storePath, "session.jsonl");
    const indexPath = join(root, "index.sqlite");
    await mkdir(storePath, { recursive: true });
    await writeFile(transcriptPath, JSON.stringify({
      type: "response_item",
      payload: { type: "message", role: "user", content: [{ type: "input_text", text: "Alpha beta and alpha-beta" }] },
    }));
    try {
      const stores: TranscriptStore[] = [{ source: "codex", kind: "jsonl", path: storePath }];
      await refreshTranscriptIndex(stores, indexPath);
      expect(searchTranscriptIndex("alpha beta", stores, indexPath)[0]?.count).toBe(1);
      expect(searchTranscriptIndex("alpha  beta", stores, indexPath)).toEqual([]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("transcript index incremental paths", () => {
  test("appends only the new tail of a growing JSONL file", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-append-"));
    const storePath = join(root, "claude");
    const transcriptPath = join(storePath, "grow.jsonl");
    const indexPath = join(root, "index.sqlite");
    const stores: TranscriptStore[] = [{ source: "claude", kind: "jsonl", path: storePath }];
    await mkdir(storePath, { recursive: true });
    await writeFile(transcriptPath, `${claudeLine("user", "first message")}\n`);
    try {
      await refreshTranscriptIndex(stores, indexPath);
      // A half-written trailing line must wait for its newline.
      await appendFile(transcriptPath, `${claudeLine("assistant", "second message")}\n{"type":"user","mess`);
      const grown = await refreshTranscriptIndex(stores, indexPath);
      expect(grown).toMatchObject({ indexed: 1, messages: 2 });
      expect(searchTranscriptIndex("second message", stores, indexPath)[0]?.count).toBe(1);
      await appendFile(transcriptPath, `age":{"role":"user","content":[{"type":"text","text":"third message"}]}}\n`);
      expect((await refreshTranscriptIndex(stores, indexPath)).messages).toBe(3);
      expect(searchTranscriptIndex("third message", stores, indexPath)[0]?.count).toBe(1);
      expect(searchTranscriptIndex("first message", stores, indexPath)[0]?.count).toBe(1);
      // A rewrite that changes the head falls back to a full reparse.
      await writeFile(transcriptPath, `${claudeLine("user", "rewritten only")}\n`);
      expect((await refreshTranscriptIndex(stores, indexPath)).messages).toBe(1);
      expect(searchTranscriptIndex("first message", stores, indexPath)).toEqual([]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("indexes OpenCode text parts incrementally by cursor and reports unreadable stores", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-opencode-"));
    const databasePath = join(root, "opencode.db");
    const indexPath = join(root, "index.sqlite");
    const database = new Database(databasePath, { create: true });
    database.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
    database.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
    database.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT)");
    database.run("INSERT INTO session VALUES (?, ?, ?, ?)", ["ses_1", "/work/demo", "Demo", Date.UTC(2026, 7, 4)]);
    database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["msg_1", "ses_1", 1, JSON.stringify({ role: "user" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)", ["part_1", "msg_1", "ses_1", 1, 1, JSON.stringify({ type: "text", text: "Needle question" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)", ["part_2", "msg_1", "ses_1", 2, 2, JSON.stringify({ type: "reasoning", text: "Needle hidden" })]);
    const stores: TranscriptStore[] = [
      { source: "opencode", kind: "sqlite", path: databasePath },
      { source: "opencode", kind: "sqlite", path: join(root, "missing.db") },
    ];
    try {
      const initial = await refreshTranscriptIndex(stores, indexPath);
      expect(initial.indexed).toBe(1);
      expect(initial.skipped).toHaveLength(1);
      expect(initial.skipped[0]?.path).toBe(join(root, "missing.db"));
      const [match] = searchTranscriptIndexMatches("needle", [stores[0]!], indexPath);
      expect(match).toMatchObject({ source: "opencode", count: 1, project: "/work/demo", date: "2026-08-04", snippets: [{ role: "user", text: "Needle question" }] });
      expect(parseOpenCodeLocator(match!.path)).toEqual({ databasePath, sessionId: "ses_1" });

      // A streamed part is re-read when OpenCode bumps its time_updated.
      database.run("UPDATE part SET time_updated = 3, data = ? WHERE id = 'part_1'", [JSON.stringify({ type: "text", text: "Needle question needle again" })]);
      expect((await refreshTranscriptIndex([stores[0]!], indexPath)).indexed).toBe(1);
      expect(searchTranscriptIndexMatches("needle", [stores[0]!], indexPath)[0]).toMatchObject({ count: 2 });
      expect((await refreshTranscriptIndex([stores[0]!], indexPath)).indexed).toBe(0);
    } finally {
      database.close();
      await rm(root, { recursive: true, force: true });
    }
  });

  test("rebuilds an index whose schema version is stale", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-schema-"));
    const indexPath = join(root, "index.sqlite");
    const stale = new Database(indexPath, { create: true });
    stale.run("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
    stale.run("INSERT INTO metadata VALUES ('schema_version', '1')");
    stale.run("CREATE TABLE files (path TEXT PRIMARY KEY)");
    stale.close();
    try {
      expect((await transcriptIndexStatus(indexPath)).schemaVersion).toBe(1);
      await refreshTranscriptIndex([], indexPath);
      expect(await transcriptIndexStatus(indexPath)).toMatchObject({ exists: true, schemaVersion: 2, files: 0, messages: 0 });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
