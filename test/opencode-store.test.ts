import { Database } from "bun:sqlite";
import { beforeAll, describe, expect, test } from "bun:test";
import { readFile, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { searchSessions } from "../src/core.ts";
import { loadOpenCodeMessages, openCodeLocator, parseOpenCodeLocator, searchOpenCodeStore } from "../src/opencode-store.ts";

const databasePath = `/tmp/dejavu-opencode-${crypto.randomUUID()}.db`;
const v2DatabasePath = `/tmp/dejavu-opencode-v2-${crypto.randomUUID()}.db`;
const hybridDatabasePath = `/tmp/dejavu-opencode-hybrid-${crypto.randomUUID()}.db`;

function createV2Tables(database: Database): void {
  database.run("CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL)");
  database.run("CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL)");
}

function insertV2Messages(database: Database, sessionId: string): void {
  const rows: Array<[string, string, object]> = [
    ["msg_v1", "user", { text: "Needle question", files: [] }],
    ["msg_v2", "assistant", { content: [
      { type: "reasoning", text: "Needle hidden" },
      { type: "text", text: "Needle answer" },
      { type: "tool", tool: "bash", state: { output: "needle tool" } },
      { type: "text", text: "Needle follow-up" },
    ] }],
    ["msg_v3", "synthetic", { text: "Needle synthetic" }],
    ["msg_v4", "compaction", { summary: "Needle summary" }],
  ];
  rows.forEach(([id, type, data], seq) => {
    database.run("INSERT INTO session_message VALUES (?, ?, ?, ?, ?, ?, ?)", [id, sessionId, type, seq, seq, seq, JSON.stringify(data)]);
  });
}

beforeAll(() => {
  const database = new Database(databasePath, { create: true });
  database.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
  database.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
  database.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)");
  database.run("INSERT INTO session VALUES (?, ?, ?, ?)", ["ses_1", "/work/demo", "Demo", Date.UTC(2026, 7, 4)]);
  database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["msg_1", "ses_1", 1, JSON.stringify({ role: "user" })]);
  database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["msg_2", "ses_1", 2, JSON.stringify({ role: "assistant" })]);
  database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["part_1", "msg_1", "ses_1", 1, JSON.stringify({ type: "text", text: "Needle question" })]);
  database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["part_2", "msg_2", "ses_1", 2, JSON.stringify({ type: "text", text: "Needle answer" })]);
  database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["part_3", "msg_2", "ses_1", 3, JSON.stringify({ type: "reasoning", text: "Needle hidden" })]);
  database.close();

  const v2 = new Database(v2DatabasePath, { create: true });
  createV2Tables(v2);
  v2.run("INSERT INTO session_v2 VALUES (?, ?, ?, ?)", ["ses_v2", "/work/next", "Next", Date.UTC(2026, 8, 1)]);
  insertV2Messages(v2, "ses_v2");
  v2.close();

  // A migrated store keeps residual legacy parts for sessions that now live in session_message.
  const hybrid = new Database(hybridDatabasePath, { create: true });
  hybrid.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
  hybrid.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
  hybrid.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)");
  createV2Tables(hybrid);
  for (const id of ["ses_old", "ses_moved"]) {
    hybrid.run("INSERT INTO session VALUES (?, ?, ?, ?)", [id, `/work/${id}`, id, Date.UTC(2026, 7, 4)]);
    hybrid.run("INSERT INTO message VALUES (?, ?, ?, ?)", [`msg_${id}`, id, 1, JSON.stringify({ role: "user" })]);
    hybrid.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", [`part_${id}`, `msg_${id}`, id, 1, JSON.stringify({ type: "text", text: "Needle legacy" })]);
  }
  hybrid.run("INSERT INTO session_v2 VALUES (?, ?, ?, ?)", ["ses_moved", "/work/ses_moved", "Moved", Date.UTC(2026, 8, 1)]);
  insertV2Messages(hybrid, "ses_moved");
  hybrid.close();
});

describe("OpenCode SQLite adapter", () => {
  test("searches visible text parts and returns a queryable locator", async () => {
    const matches = await searchOpenCodeStore("needle", databasePath, 10, 3);
    expect(matches).toHaveLength(1);
    expect(matches[0]).toMatchObject({ source: "opencode", count: 2, project: "/work/demo" });
    expect(parseOpenCodeLocator(matches[0]!.path)).toEqual({ databasePath, sessionId: "ses_1" });
  });

  test("loads ordered user and assistant text", async () => {
    const messages = await loadOpenCodeMessages(openCodeLocator(databasePath, "ses_1"));
    expect(messages.map((message) => [message.role, message.content[0]?.text])).toEqual([
      ["user", "Needle question"], ["assistant", "Needle answer"],
    ]);
  });

  test("searches and loads visible user and assistant text from a v2 session_message store", async () => {
    const [match, ...rest] = await searchOpenCodeStore("needle", v2DatabasePath, 10, 5);
    expect(rest).toEqual([]);
    expect(match).toMatchObject({ source: "opencode", count: 3, project: "/work/next", date: "2026-09-01" });
    expect(match!.snippets.map((snippet) => [snippet.role, snippet.text]).sort()).toEqual([
      ["assistant", "Needle answer"], ["assistant", "Needle follow-up"], ["user", "Needle question"],
    ]);
    expect(parseOpenCodeLocator(match!.path)).toEqual({ databasePath: v2DatabasePath, sessionId: "ses_v2" });

    const messages = await loadOpenCodeMessages(match!.path);
    expect(messages.map((message) => [message.role, message.content.map((item) => item.text)])).toEqual([
      ["user", ["Needle question"]], ["assistant", ["Needle answer", "Needle follow-up"]],
    ]);
  });

  test("reads each session of a hybrid store from one schema only", async () => {
    const matches = await searchOpenCodeStore("needle", hybridDatabasePath, 10, 5);
    const bySession = Object.fromEntries(matches.map((match) => [parseOpenCodeLocator(match.path).sessionId, match.count]));
    expect(bySession).toEqual({ ses_old: 1, ses_moved: 3 });
    expect((await loadOpenCodeMessages(openCodeLocator(hybridDatabasePath, "ses_old"))).map((message) => message.content[0]?.text)).toEqual(["Needle legacy"]);
    expect(await loadOpenCodeMessages(openCodeLocator(hybridDatabasePath, "ses_moved"))).toHaveLength(2);
  });

  test("names an unsupported OpenCode schema", async () => {
    const emptyPath = `/tmp/dejavu-opencode-empty-${crypto.randomUUID()}.db`;
    new Database(emptyPath, { create: true }).close();
    const result = await searchSessions("needle", { source: "opencode", noIndex: true }, {
      discoverStores: async () => [{ source: "opencode", kind: "sqlite", path: emptyPath }],
    });
    expect(result.skippedStores[0]?.error).toContain("unsupported OpenCode schema");
  });

  test("reads a WAL opencode store that has no -shm file, without changing the database", async () => {
    const nextDatabase = join(homedir(), ".local", "share", "opencode", "opencode-next.db");
    if (!(await Bun.file(nextDatabase).exists())) return;
    if (await Bun.file(`${nextDatabase}-shm`).exists()) return;
    const beforeStat = await stat(nextDatabase);
    const beforeBytes = await readFile(nextDatabase);

    const result = await searchSessions("dejavu-sqlite-cantopen-regression", { source: "opencode" }, {
      discoverStores: async () => [{ source: "opencode", kind: "sqlite", path: nextDatabase }],
    });

    const afterStat = await stat(nextDatabase);
    const afterBytes = await readFile(nextDatabase);
    expect(result.matches).toEqual([]);
    expect(result.skippedStores).toEqual([]);
    expect(await Bun.file(`${nextDatabase}-shm`).exists()).toBe(false);
    expect({ size: afterStat.size, mtimeMs: afterStat.mtimeMs }).toEqual({ size: beforeStat.size, mtimeMs: beforeStat.mtimeMs });
    expect(afterBytes).toEqual(beforeBytes);
  });
});
