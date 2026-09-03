import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openCodeLocator } from "../src/opencode-store.ts";
import { parseDropList, redactNode, scrubPatterns, scrubTranscript } from "../src/transcript-scrub.ts";
import { loadTranscriptEvents } from "../src/transcript-view.ts";

function claudeFile(): string {
  const root = mkdtempSync(join(tmpdir(), "dejavu-scrub-"));
  const dir = join(root, ".claude", "projects", "-work-demo");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "s.jsonl");
  const lines = [
    { uuid: "u1", parentUuid: null, type: "user", message: { role: "user", content: "the host bohrium is down\nplease check" } },
    { uuid: "a1", parentUuid: "u1", type: "assistant", message: { role: "assistant", content: [
      { type: "thinking", thinking: "bohrium again" },
      { type: "text", text: "checking" },
      { type: "tool_use", id: "t1", name: "Bash", input: { command: "ssh bohrium uptime" } },
    ] } },
    { uuid: "r1", parentUuid: "a1", type: "user", toolUseResult: { stdout: "bohrium up 3 days" }, message: { role: "user", content: [{ type: "tool_result", tool_use_id: "t1", content: "bohrium up 3 days" }] } },
    { uuid: "a2", parentUuid: "r1", type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "all good" }] } },
    { uuid: "dead", parentUuid: "u1", type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "dead branch mentions bohrium" }] } },
    { type: "last-prompt", leafUuid: "a2", lastPrompt: "the host bohrium is down" },
  ];
  writeFileSync(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
  return path;
}

describe("scrubTranscript on Claude JSONL", () => {
  test("dry run reports without writing", async () => {
    const path = claudeFile();
    const before = readFileSync(path, "utf8");
    const result = await scrubTranscript(path, { drop: [3], patterns: ["bohrium"], dryRun: true }, { now: () => 1000 });
    expect(result).toMatchObject({ dryRun: true, backup: null, droppedEvents: [3, 4], changedRecords: 5, patternLines: 4 });
    expect(readFileSync(path, "utf8")).toBe(before);
  });

  test("dropping a tool call redacts the call, its result, and the sibling toolUseResult copy", async () => {
    const path = claudeFile();
    const result = await scrubTranscript(path, { drop: [3] }, { now: () => 1000 });
    expect(result.droppedEvents).toEqual([3, 4]);
    expect(result.backup).toBe(`${path}.bak-1`);
    expect(existsSync(result.backup!)).toBe(true);
    const lines = readFileSync(path, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(lines[1].message.content[2]).toEqual({ type: "tool_use", id: "t1", name: "Bash", input: { command: "[redacted]" } });
    expect(lines[1].message.content[1]).toEqual({ type: "text", text: "checking" });
    expect(lines[2].message.content[0]).toEqual({ type: "tool_result", tool_use_id: "t1", content: "[redacted]" });
    expect(lines[2].toolUseResult).toEqual({ stdout: "[redacted]" });
    const { events } = await loadTranscriptEvents(path, "claude");
    expect(events[3]).toMatchObject({ kind: "tool_call", input: { command: "[redacted]" } });
    expect(events[4]).toMatchObject({ kind: "tool_result", output: "[redacted]" });
  });

  test("dropping a user turn keeps the parent chain and replaces the whole string content", async () => {
    const path = claudeFile();
    await scrubTranscript(path, { drop: [0], placeholder: "please continue" });
    const lines = readFileSync(path, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(lines[0]).toEqual({ uuid: "u1", parentUuid: null, type: "user", message: { role: "user", content: "please continue" } });
    expect((await loadTranscriptEvents(path, "claude")).events).toHaveLength(6);
  });

  test("patterns remove matching lines from every record, including dead branches and last-prompt markers", async () => {
    const path = claudeFile();
    const result = await scrubTranscript(path, { patterns: ["BOHRIUM"] });
    expect(result.patternLines).toBe(7);
    const raw = readFileSync(path, "utf8");
    expect(raw.toLowerCase()).not.toContain("bohrium");
    const lines = raw.trim().split("\n").map((line) => JSON.parse(line));
    expect(lines[0].message.content).toBe("please check");
    expect(lines[1].message.content[0]).toEqual({ type: "thinking", thinking: "[redacted]" });
    expect(lines[1].message.content[2].input.command).toBe("[redacted]");
    expect(lines[2].toolUseResult.stdout).toBe("[redacted]");
    expect(lines[4].message.content[0].text).toBe("[redacted]");
    expect(lines[5].lastPrompt).toBe("[redacted]");
  });

  test("rejects unknown event numbers and empty requests", async () => {
    const path = claudeFile();
    await expect(scrubTranscript(path, { drop: [99] })).rejects.toThrow(/no event #99/);
    await expect(scrubTranscript(path, {})).rejects.toThrow(/at least one/);
  });
});

describe("scrubTranscript on Codex JSONL", () => {
  test("redacts function calls with their outputs and leaves other records untouched", async () => {
    const root = mkdtempSync(join(tmpdir(), "dejavu-scrub-"));
    const dir = join(root, ".codex", "sessions", "2026", "09", "02");
    mkdirSync(dir, { recursive: true });
    const path = join(dir, "rollout.jsonl");
    const lines = [
      { type: "session_meta", payload: { cwd: "/work" } },
      { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "run it" }] } },
      { type: "response_item", payload: { type: "function_call", name: "exec", arguments: "{\"cmd\":\"cat secret\"}", call_id: "c1" } },
      { type: "response_item", payload: { type: "function_call_output", call_id: "c1", output: "TOKEN=abc" } },
      { type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "done" }] } },
    ];
    writeFileSync(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
    const result = await scrubTranscript(path, { drop: [1] });
    expect(result.droppedEvents).toEqual([1, 2]);
    const after = readFileSync(path, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(after[2].payload).toEqual({ type: "function_call", name: "exec", arguments: "[redacted]", call_id: "c1" });
    expect(after[3].payload).toEqual({ type: "function_call_output", call_id: "c1", output: "[redacted]" });
    expect(after[1]).toEqual(lines[1]);
    expect(after[4]).toEqual(lines[4]);
  });
});

describe("scrubTranscript on OpenCode SQLite", () => {
  test("updates the targeted part rows and pattern matches inside a transaction after copying the database", async () => {
    const databasePath = `/tmp/dejavu-scrub-${crypto.randomUUID()}.db`;
    const database = new Database(databasePath, { create: true });
    database.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
    database.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
    database.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)");
    database.run("INSERT INTO session VALUES (?, ?, ?, ?)", ["ses", "/work/oc", "OC", 1]);
    database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["m1", "ses", 1, JSON.stringify({ role: "user", summary: { title: "bohrium outage" } })]);
    database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["m2", "ses", 2, JSON.stringify({ role: "assistant" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p1", "m1", "ses", 1, JSON.stringify({ type: "text", text: "check bohrium\nand the rest" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p2", "m2", "ses", 2, JSON.stringify({ type: "tool", tool: "bash", callID: "b1", state: { status: "completed", input: { command: "ssh bohrium" }, output: "up", title: "ssh bohrium" } })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p3", "m2", "ses", 3, JSON.stringify({ type: "text", text: "all good" })]);
    database.close();
    try {
      const locator = openCodeLocator(databasePath, "ses");
      const result = await scrubTranscript(locator, { drop: [1], patterns: ["bohrium"] }, { now: () => 7000 });
      expect(result).toMatchObject({ source: "opencode", droppedEvents: [1, 2], backup: `${databasePath}.bak-7`, changedRecords: 3 });
      expect(existsSync(result.backup!)).toBe(true);
      const reopened = new Database(databasePath, { readonly: true });
      const parts = Object.fromEntries(reopened.query<{ id: string; data: string }, []>("SELECT id, data FROM part").all().map((row) => [row.id, JSON.parse(row.data)]));
      const messages = Object.fromEntries(reopened.query<{ id: string; data: string }, []>("SELECT id, data FROM message").all().map((row) => [row.id, JSON.parse(row.data)]));
      reopened.close();
      expect(parts.p1.text).toBe("and the rest");
      expect(parts.p2.state).toEqual({ status: "completed", input: { command: "[redacted]" }, output: "[redacted]", title: "[redacted]" });
      expect(parts.p3.text).toBe("all good");
      expect(messages.m1.summary.title).toBe("[redacted]");
      const { events } = await loadTranscriptEvents(locator, "opencode");
      expect(events.map((event) => event.kind)).toEqual(["user", "tool_call", "tool_result", "assistant"]);
    } finally {
      rmSync(databasePath, { force: true });
      rmSync(`${databasePath}.bak-7`, { force: true });
    }
  });
});

describe("helpers", () => {
  test("parseDropList accepts numbers, ranges, and comma lists", () => {
    expect(parseDropList(["4", "7-9", "1,2"])).toEqual([4, 7, 8, 9, 1, 2]);
    expect(() => parseDropList(["9-7"])).toThrow(/backwards/);
    expect(() => parseDropList(["x"])).toThrow(/event numbers/);
  });

  test("redactNode keeps structure and ids while replacing text and payload fields", () => {
    expect(redactNode({ type: "tool_use", id: "t1", name: "Bash", input: { command: "ls", nested: { description: "x" } } }, "[r]"))
      .toEqual({ type: "tool_use", id: "t1", name: "Bash", input: { command: "[r]", nested: { description: "[r]" } } });
  });

  test("scrubPatterns drops only matching lines and falls back to the placeholder", () => {
    const counter = { lines: 0 };
    expect(scrubPatterns({ a: "keep\nDROP me\nkeep too", b: ["drop"], c: 3 }, ["drop"], "[r]", counter))
      .toEqual({ a: "keep\nkeep too", b: ["[r]"], c: 3 });
    expect(counter.lines).toBe(2);
  });

  test("scrubPatterns renames object keys that contain the pattern, such as snapshot paths", () => {
    const counter = { lines: 0 };
    expect(scrubPatterns({ backups: { "/Users/secret/a.md": 1, "/Users/secret/b.md": 2, "/other": 3 } }, ["secret"], "[r]", counter))
      .toEqual({ backups: { "[r]": 1, "[r]-2": 2, "/other": 3 } });
    expect(counter.lines).toBe(2);
  });
});
