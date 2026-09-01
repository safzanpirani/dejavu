import { Database } from "bun:sqlite";
import { beforeAll, describe, expect, test } from "bun:test";
import { readFile, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { searchSessions } from "../src/core.ts";
import { loadOpenCodeMessages, openCodeLocator, parseOpenCodeLocator, searchOpenCodeStore } from "../src/opencode-store.ts";

const databasePath = `/tmp/deja-opencode-${crypto.randomUUID()}.db`;

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

  test("reports the real opencode-next SQLITE_CANTOPEN without changing the database", async () => {
    const nextDatabase = join(homedir(), ".local", "share", "opencode", "opencode-next.db");
    if (!(await Bun.file(nextDatabase).exists())) return;
    const beforeStat = await stat(nextDatabase);
    const beforeBytes = await readFile(nextDatabase);

    const result = await searchSessions("deja-sqlite-cantopen-regression", { source: "opencode" }, {
      discoverStores: async () => [{ source: "opencode", kind: "sqlite", path: nextDatabase }],
    });

    const afterStat = await stat(nextDatabase);
    const afterBytes = await readFile(nextDatabase);
    expect(result.matches).toEqual([]);
    expect(result.skippedStores).toHaveLength(1);
    expect(result.skippedStores[0]).toMatchObject({ source: "opencode", path: nextDatabase });
    expect(result.skippedStores[0]?.error.toLowerCase()).toContain("unable to open database file");
    expect({ size: afterStat.size, mtimeMs: afterStat.mtimeMs }).toEqual({ size: beforeStat.size, mtimeMs: beforeStat.mtimeMs });
    expect(afterBytes).toEqual(beforeBytes);
  });
});
